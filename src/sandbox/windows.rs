//! Native Windows write-restriction runner for the sandbox seam.
//!
//! This deliberately provides a write boundary only. The restricted token
//! retains the logon SID and Everyone so PowerShell, DLL loading, and CNG keep
//! working; ambient Everyone grants therefore remain a documented limit, as
//! do existing NTFS hard-link aliases into approved roots. It does not restrict
//! reads, networking, or process visibility.

use std::{
    ffi::{c_void, OsStr, OsString},
    fs, io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetHandleInformation, LocalFree, SetHandleInformation, ERROR_SUCCESS, HANDLE,
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
    },
    Security::{
        Authorization::{
            GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, GRANT_ACCESS,
            NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
            TRUSTEE_W,
        },
        CopySid, CreateRestrictedToken, CreateWellKnownSid, EqualSid, GetAce, GetLengthSid,
        GetTokenInformation, IsValidSid, SetTokenInformation, TokenDefaultDacl, TokenGroups,
        TokenOwner, TokenUser, WinWorldSid, ACCESS_ALLOWED_ACE, ACL, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, LUA_TOKEN, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_QUERY, WRITE_RESTRICTED,
    },
    Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        GetVolumeInformationByHandleW, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC,
    },
    System::{
        Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS, FILE_READ_ONLY_VOLUME, SE_GROUP_LOGON_ID,
        },
        Threading::{
            CreateMutexW, CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess,
            GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken, ReleaseMutex,
            ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
            PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES,
            STARTUPINFOEXW,
        },
    },
};

use super::{
    canonical_directory, sandbox_error, EffectiveSandboxRequest, RunnerRules, SandboxDenial,
    SandboxEnforcement, SandboxMode, SandboxPlan, SandboxProvider,
};
use crate::{subprocess::WindowsJob, TessivumError};

const RUNNER_ARG: &str = "__tessivum-windows-acl-run";
const RUNNER_SIGNATURE: &str = "windows-acl-run";
const RUNNER_FAILURE: i32 = 127;
const PAYLOAD_VERSION: u8 = 1;
const MAX_PAYLOAD_WCHARS: usize = 24 * 1024;
const MAX_ARGV: usize = 256;
const MAX_ARG_WCHARS: usize = 8 * 1024;
const TEMP_PREFIX: &str = "tessivum-acl-";
const MARKER: &str = ".tessivum-acl-owner";
const MARKER_MAGIC: &str = "tessivum-windows-acl-v1";
const GRANT_MASK: u32 = 0x0011_0156;
const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
const SECURITY_MAX_SID_SIZE: usize = 68;

#[derive(Clone, Debug)]
pub(super) struct WindowsAclProvider {
    runner: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunnerInput {
    version: u8,
    mode: SandboxMode,
    workspace: PathBuf,
    write_roots: Vec<PathBuf>,
    argv: Vec<String>,
}

impl WindowsAclProvider {
    pub(super) fn detect() -> Option<Self> {
        resolve_runner().map(|runner| Self { runner })
    }
}

impl SandboxProvider for WindowsAclProvider {
    fn confine(
        &self,
        request: &EffectiveSandboxRequest,
        argv: &[String],
    ) -> Result<SandboxPlan, TessivumError> {
        if !matches!(
            request.mode,
            SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite
        ) {
            return Err(unavailable("unsupported Windows ACL mode"));
        }
        validate_bounded_argv(argv)?;
        let preflight = (|| -> io::Result<()> {
            let token = open_current_token()?;
            let user_sid = token_sid(token.0, TokenUser, false)?;
            PinnedDirectory::open(&request.workspace, &user_sid, false)?;
            for root in &request.write_roots {
                PinnedDirectory::open(root, &user_sid, true)?;
            }
            Ok(())
        })();
        preflight.map_err(|error| unavailable(&error.to_string()))?;
        let payload = serde_json::to_string(&RunnerInput {
            version: PAYLOAD_VERSION,
            mode: request.mode,
            workspace: request.workspace.clone(),
            write_roots: request.write_roots.clone(),
            argv: argv.to_vec(),
        })
        .map_err(|error| unavailable(&format!("runner input is not Unicode JSON: {error}")))?;
        if payload.encode_utf16().count() > MAX_PAYLOAD_WCHARS {
            return Err(unavailable(
                "runner input exceeds the Windows command-line bound",
            ));
        }
        Ok(SandboxPlan {
            argv: vec![
                self.runner.to_string_lossy().into_owned(),
                RUNNER_ARG.into(),
                payload,
            ],
            enforcement: SandboxEnforcement::Full,
            denial: Some(SandboxDenial {
                code: "SANDBOX_DENIED".into(),
                message: "Windows ACL sandbox refused to start the command".into(),
            }),
            runner_rules: RunnerRules {
                denial_exit_codes: Some([RUNNER_FAILURE].into_iter().collect()),
                informational_stderr: Default::default(),
            },
        })
    }
}

pub(super) fn dispatch(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(OsStr::new(RUNNER_ARG)) {
        return None;
    }
    let payload = match (args.next(), args.next()) {
        (Some(payload), None) => payload,
        _ => return Some(fail("expected exactly one structured runner argument")),
    };
    let Some(payload) = payload.to_str() else {
        return Some(fail("runner input is not Unicode"));
    };
    if payload.encode_utf16().count() > MAX_PAYLOAD_WCHARS {
        return Some(fail("runner input exceeds the command-line bound"));
    }
    Some(match serde_json::from_str::<RunnerInput>(payload) {
        Ok(input) => match run(input) {
            Ok(code) => code as i32,
            Err(error) => fail(&error.to_string()),
        },
        Err(error) => fail(&format!("invalid runner input: {error}")),
    })
}

fn fail(message: &str) -> i32 {
    eprintln!("{RUNNER_SIGNATURE}: {message}");
    RUNNER_FAILURE
}

fn run(mut input: RunnerInput) -> io::Result<u32> {
    if input.version != PAYLOAD_VERSION {
        return Err(invalid("unsupported runner input version"));
    }
    if !matches!(
        input.mode,
        SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite
    ) {
        return Err(invalid("runner accepts only confined modes"));
    }
    validate_native_argv(&input.argv)?;

    let workspace = canonical_directory(&input.workspace, "workspace").map_err(as_io)?;
    if workspace != input.workspace {
        return Err(invalid("runner workspace is not canonical"));
    }
    let mut roots = Vec::with_capacity(input.write_roots.len());
    for root in &input.write_roots {
        let canonical = canonical_directory(root, "write root").map_err(as_io)?;
        if canonical != *root || !canonical.starts_with(&workspace) {
            return Err(invalid(
                "runner write root is not canonical inside the workspace",
            ));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    if input.mode == SandboxMode::ReadOnly && !roots.is_empty() {
        return Err(invalid("read-only runner input carries write roots"));
    }
    if input.mode == SandboxMode::WorkspaceWrite && roots.is_empty() {
        return Err(invalid("workspace-write requires an approved write root"));
    }
    input.write_roots = roots;

    let current_token = open_current_token()?;
    let user_sid = token_sid(current_token.0, TokenUser, false)?;
    cleanup_stale(&user_sid)?;

    let temp_base = fs::canonicalize(std::env::temp_dir())?;
    if is_reparse(&temp_base)? || contains(&workspace, &temp_base) {
        return Err(invalid(
            "private temp parent must be non-reparse and outside workspace",
        ));
    }
    let temp = create_private_temp(&temp_base, &user_sid)?;
    let result = execute(&input, &workspace, &user_sid, &current_token, &temp);
    let cleanup = temp.cleanup(&user_sid);
    match (result, cleanup) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn execute(
    input: &RunnerInput,
    workspace: &Path,
    user_sid: &Sid,
    current_token: &OwnedHandle,
    temp: &PrivateTemp,
) -> io::Result<u32> {
    let root_sids: Vec<Sid> = input
        .write_roots
        .iter()
        .map(|root| capability_sid(root.as_os_str(), false))
        .collect();
    let temp_sid = capability_sid(temp.path.as_os_str(), true);

    let mut pinned = Vec::new();
    pinned.push(PinnedDirectory::open(
        workspace,
        user_sid,
        input
            .write_roots
            .iter()
            .any(|root| same_path(root, workspace)),
    )?);
    for root in &input.write_roots {
        if !pinned.iter().any(|item| same_path(&item.path, root)) {
            pinned.push(PinnedDirectory::open(root, user_sid, true)?);
        }
    }
    if input
        .write_roots
        .iter()
        .any(|root| contains(root, &temp.path) || contains(&temp.path, root))
    {
        return Err(invalid("private temp and writable roots intersect"));
    }

    if input.mode == SandboxMode::WorkspaceWrite {
        for (root, sid) in input.write_roots.iter().zip(&root_sids) {
            let directory = pinned
                .iter()
                .find(|directory| same_path(&directory.path, root))
                .expect("every validated write root is pinned");
            grant(directory, sid)?;
        }
    }
    grant(&temp.directory, &temp_sid)?;

    let restricted = (|| {
        let logon_sid = token_sid(current_token.0, TokenGroups, true)?;
        let world_sid = well_known_sid(WinWorldSid)?;
        let write_sids: Vec<&Sid> = root_sids.iter().chain(std::iter::once(&temp_sid)).collect();
        let token = restricted_token(current_token.0, &logon_sid, &world_sid, &write_sids)?;
        set_default_dacl(token.0, &temp_sid)?;
        spawn_wait(token.0, &input.argv, workspace, &temp.path)
    })();

    let revoke = revoke(&temp.directory, &temp_sid);
    match (restricted, revoke) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn spawn_wait(token: HANDLE, argv: &[String], cwd: &Path, temp: &Path) -> io::Result<u32> {
    injected(2)?;
    let handles = [
        unsafe { GetStdHandle(STD_INPUT_HANDLE) },
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
        unsafe { GetStdHandle(STD_ERROR_HANDLE) },
    ];
    if handles
        .iter()
        .any(|handle| handle.is_null() || *handle == INVALID_HANDLE_VALUE)
    {
        return Err(last_error("GetStdHandle"));
    }
    let inherit = InheritHandles::enable(&handles)?;
    let mut attribute_bytes = 0usize;
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_bytes);
    }
    if attribute_bytes == 0 {
        return Err(last_error("InitializeProcThreadAttributeList(size)"));
    }
    let mut storage = vec![0usize; attribute_bytes.div_ceil(size_of::<usize>())];
    let attribute_list = storage.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_bytes) } == 0
    {
        return Err(last_error("InitializeProcThreadAttributeList"));
    }
    let attributes = AttributeList(attribute_list);
    if unsafe {
        UpdateProcThreadAttribute(
            attributes.0,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of::<HANDLE>() * handles.len(),
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(last_error("UpdateProcThreadAttribute(handle list)"));
    }

    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.0;
    let mut info = PROCESS_INFORMATION::default();
    let command_line = build_command_line(argv);
    let mut command = wide(OsStr::new(&command_line));
    let cwd = wide(crate::process_path(cwd).as_os_str());
    let environment = child_environment(temp);
    injected(3)?;
    let created = unsafe {
        CreateProcessAsUserW(
            token,
            null(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    drop(inherit);
    if created == 0 {
        return Err(last_error("CreateProcessAsUserW"));
    }
    let process = OwnedHandle::new(info.hProcess, "CreateProcessAsUserW process")?;
    let thread = OwnedHandle::new(info.hThread, "CreateProcessAsUserW thread")?;
    #[cfg(test)]
    FAIL_PROCESS_ID.store(info.dwProcessId, std::sync::atomic::Ordering::SeqCst);
    if let Err(error) = injected(4) {
        unsafe {
            TerminateProcess(process.0, 1);
            WaitForSingleObject(process.0, INFINITE);
        }
        return Err(error);
    }

    let job = match unsafe { WindowsJob::assign_raw(process.0) } {
        Ok(job) => job,
        Err(error) => {
            unsafe {
                TerminateProcess(process.0, 1);
                WaitForSingleObject(process.0, INFINITE);
            }
            return Err(error);
        }
    };
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        let error = last_error("ResumeThread");
        job.terminate();
        unsafe {
            WaitForSingleObject(process.0, INFINITE);
        }
        return Err(error);
    }
    drop(thread);
    let waited = unsafe { WaitForSingleObject(process.0, INFINITE) };
    if waited == WAIT_FAILED {
        job.terminate();
        return Err(last_error("WaitForSingleObject"));
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        job.terminate();
        return Err(last_error("GetExitCodeProcess"));
    }
    job.terminate();
    job.wait_for_exit()?;
    drop(job);
    Ok(code)
}

struct InheritHandles(Vec<(HANDLE, u32)>);

impl InheritHandles {
    fn enable(handles: &[HANDLE]) -> io::Result<Self> {
        let mut guard = Self(Vec::new());
        for &handle in handles {
            if guard.0.iter().any(|(seen, _)| *seen == handle) {
                continue;
            }
            let mut flags = 0;
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(last_error("GetHandleInformation"));
            }
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(last_error("SetHandleInformation(enable inherit)"));
            }
            guard.0.push((handle, flags));
        }
        Ok(guard)
    }
}

impl Drop for InheritHandles {
    fn drop(&mut self) {
        for (handle, flags) in self.0.drain(..) {
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}

struct AttributeList(*mut c_void);

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0) }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE, label: &str) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(last_error(label))
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct NamedMutex(OwnedHandle);

impl NamedMutex {
    fn acquire(path: &Path) -> io::Result<Self> {
        let digest = Sha256::digest(path.to_string_lossy().to_lowercase().as_bytes());
        Self::acquire_name(&format!("Global\\TessivumAcl-{}", hex(&digest[..16])))
    }

    fn acquire_recovery(user_sid: &Sid) -> io::Result<Self> {
        Self::acquire_name(&format!(
            "Global\\TessivumAcl-Recovery-{}",
            hex(user_sid.bytes())
        ))
    }

    fn acquire_name(name: &str) -> io::Result<Self> {
        // Files and temp roots are shared across this user's Windows logon sessions.
        let name = wide(OsStr::new(name));
        let handle = OwnedHandle::new(
            unsafe { CreateMutexW(null(), 0, name.as_ptr()) },
            "CreateMutexW",
        )?;
        match unsafe { WaitForSingleObject(handle.0, INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            WAIT_FAILED => Err(last_error("WaitForSingleObject(mutex)")),
            _ => Err(io::Error::other("unexpected named mutex wait result")),
        }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0 .0);
        }
    }
}

struct PinnedDirectory {
    path: PathBuf,
    handle: OwnedHandle,
}

impl PinnedDirectory {
    fn open(path: &Path, user_sid: &Sid, write_dac: bool) -> io::Result<Self> {
        if is_reparse(path)? {
            return Err(invalid("sandbox directory is a reparse point"));
        }
        let desired = FILE_READ_ATTRIBUTES | READ_CONTROL | if write_dac { WRITE_DAC } else { 0 };
        let path_w = wide(path.as_os_str());
        let handle = OwnedHandle::new(
            unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    desired,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            },
            "CreateFileW(directory)",
        )?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
            return Err(last_error("GetFileInformationByHandle"));
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(invalid("sandbox path is not a non-reparse directory"));
        }
        validate_volume(handle.0)?;
        let final_path = final_path(handle.0)?;
        if !same_path(&final_path, path) {
            return Err(invalid("directory handle does not match canonical path"));
        }
        let (owner, _, descriptor) = security(handle.0)?;
        let owner_matches = (|| -> io::Result<bool> {
            if owner.is_null() {
                return Ok(false);
            }
            if unsafe { EqualSid(owner, user_sid.ptr()) } != 0 {
                return Ok(true);
            }
            // Elevated Windows tokens may create directories owned by their default
            // owner group rather than TokenUser. Do not accept arbitrary group owners.
            let token = open_current_token()?;
            let default_owner = token_sid(token.0, TokenOwner, false)?;
            Ok(unsafe { EqualSid(owner, default_owner.ptr()) } != 0)
        })();
        drop_descriptor(descriptor)?;
        if !owner_matches? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sandbox directory is not owned by the current user or token owner",
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            handle,
        })
    }
}

fn grant(directory: &PinnedDirectory, sid: &Sid) -> io::Result<()> {
    injected(1)?;
    let _lock = NamedMutex::acquire(&directory.path)?;
    if exact_grant(directory.handle.0, sid)? {
        return Ok(());
    }
    edit_acl(directory.handle.0, sid, GRANT_ACCESS, GRANT_MASK)?;
    if !exact_grant(directory.handle.0, sid)? {
        return Err(io::Error::other("DACL grant verification failed"));
    }
    Ok(())
}

fn revoke(directory: &PinnedDirectory, sid: &Sid) -> io::Result<()> {
    let _lock = NamedMutex::acquire(&directory.path)?;
    edit_acl(directory.handle.0, sid, REVOKE_ACCESS, 0)?;
    if acl_has_sid(directory.handle.0, sid)? {
        return Err(io::Error::other("DACL revoke verification failed"));
    }
    Ok(())
}

fn edit_acl(handle: HANDLE, sid: &Sid, mode: i32, mask: u32) -> io::Result<()> {
    let (_, old_acl, descriptor) = security(handle)?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.ptr().cast(),
        },
    };
    let mut new_acl: *mut ACL = null_mut();
    let merged = unsafe { SetEntriesInAclW(1, &entry, old_acl, &mut new_acl) };
    let descriptor_free = drop_descriptor(descriptor);
    if merged != ERROR_SUCCESS {
        descriptor_free?;
        return Err(win32_error("SetEntriesInAclW", merged));
    }
    descriptor_free?;
    if new_acl.is_null() {
        return Err(io::Error::other("SetEntriesInAclW returned a null ACL"));
    }
    let applied = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_acl,
            null_mut(),
        )
    };
    let free = unsafe { LocalFree(new_acl.cast()) };
    if applied != ERROR_SUCCESS {
        return Err(win32_error("SetSecurityInfo", applied));
    }
    if !free.is_null() {
        return Err(last_error("LocalFree(ACL)"));
    }
    Ok(())
}

fn security(handle: HANDLE) -> io::Result<(PSID, *mut ACL, *mut c_void)> {
    let mut owner: PSID = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if result == ERROR_SUCCESS {
        Ok((owner, dacl, descriptor))
    } else {
        Err(win32_error("GetSecurityInfo", result))
    }
}

fn exact_grant(handle: HANDLE, sid: &Sid) -> io::Result<bool> {
    acl_entries(handle, |ace| unsafe {
        u32::from((*ace).Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
            && u32::from((*ace).Header.AceFlags) == OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
            && (*ace).Mask == GRANT_MASK
            && EqualSid(std::ptr::addr_of_mut!((*ace).SidStart).cast(), sid.ptr()) != 0
    })
}

fn acl_has_sid(handle: HANDLE, sid: &Sid) -> io::Result<bool> {
    acl_entries(handle, |ace| unsafe {
        u32::from((*ace).Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
            && EqualSid(std::ptr::addr_of_mut!((*ace).SidStart).cast(), sid.ptr()) != 0
    })
}

fn acl_entries(
    handle: HANDLE,
    matches: impl Fn(*mut ACCESS_ALLOWED_ACE) -> bool,
) -> io::Result<bool> {
    let (_, acl, descriptor) = security(handle)?;
    let result = if acl.is_null() {
        false
    } else {
        let count = unsafe { (*acl).AceCount };
        let mut found = false;
        for index in 0..u32::from(count) {
            let mut raw = null_mut();
            if unsafe { GetAce(acl, index, &mut raw) } == 0 {
                let error = last_error("GetAce");
                drop_descriptor(descriptor)?;
                return Err(error);
            }
            if matches(raw.cast()) {
                found = true;
                break;
            }
        }
        found
    };
    drop_descriptor(descriptor)?;
    Ok(result)
}

fn open_current_token() -> io::Result<OwnedHandle> {
    let mut token = null_mut();
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY,
            &mut token,
        )
    } == 0
    {
        return Err(last_error("OpenProcessToken"));
    }
    OwnedHandle::new(token, "OpenProcessToken")
}

struct Sid {
    words: Vec<u32>,
    length: usize,
}

impl Sid {
    fn ptr(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast(), self.length) }
    }
}

fn token_sid(token: HANDLE, class: i32, logon: bool) -> io::Result<Sid> {
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(last_error("GetTokenInformation(size)"));
    }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation"));
    }
    let source = if logon {
        let groups = buffer
            .as_ptr()
            .cast::<windows_sys::Win32::Security::TOKEN_GROUPS>();
        let count = unsafe { (*groups).GroupCount } as usize;
        let first = unsafe { std::ptr::addr_of!((*groups).Groups).cast::<SID_AND_ATTRIBUTES>() };
        (0..count)
            .map(|index| unsafe { *first.add(index) })
            .find(|entry| entry.Attributes & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32)
            .map(|entry| entry.Sid)
            .ok_or_else(|| io::Error::other("current token has no logon SID"))?
    } else if class == TokenOwner {
        unsafe {
            (*(buffer
                .as_ptr()
                .cast::<windows_sys::Win32::Security::TOKEN_OWNER>()))
            .Owner
        }
    } else {
        unsafe {
            (*(buffer
                .as_ptr()
                .cast::<windows_sys::Win32::Security::TOKEN_USER>()))
            .User
            .Sid
        }
    };
    copy_sid(source)
}

fn copy_sid(source: PSID) -> io::Result<Sid> {
    if source.is_null() || unsafe { IsValidSid(source) } == 0 {
        return Err(last_error("IsValidSid"));
    }
    let length = unsafe { GetLengthSid(source) } as usize;
    if length == 0 || length > SECURITY_MAX_SID_SIZE {
        return Err(io::Error::other("invalid SID length"));
    }
    let mut words = vec![0u32; length.div_ceil(size_of::<u32>())];
    if unsafe { CopySid(length as u32, words.as_mut_ptr().cast(), source) } == 0 {
        return Err(last_error("CopySid"));
    }
    Ok(Sid { words, length })
}

fn well_known_sid(kind: i32) -> io::Result<Sid> {
    let mut words = vec![0u32; SECURITY_MAX_SID_SIZE.div_ceil(size_of::<u32>())];
    let mut length = SECURITY_MAX_SID_SIZE as u32;
    if unsafe { CreateWellKnownSid(kind, null_mut(), words.as_mut_ptr().cast(), &mut length) } == 0
    {
        return Err(last_error("CreateWellKnownSid"));
    }
    Ok(Sid {
        words,
        length: length as usize,
    })
}

fn capability_sid(path: &OsStr, temp: bool) -> Sid {
    let mut hasher = Sha256::new();
    if temp {
        hasher.update(b"temp\0");
    }
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let modulus = (1u32 << 30) - 1;
    let first = u32::from_le_bytes(digest[0..4].try_into().unwrap()) % modulus + 1;
    let second = u32::from_le_bytes(digest[4..8].try_into().unwrap()) % modulus + 1;
    let subs: &[u32] = if temp {
        &[first, second, 1]
    } else {
        &[first, second]
    };
    let length = 8 + subs.len() * 4;
    let mut words = vec![0u32; length.div_ceil(4)];
    let bytes = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), length) };
    bytes[0] = 1;
    bytes[1] = subs.len() as u8;
    bytes[7] = 4;
    for (index, value) in subs.iter().enumerate() {
        bytes[8 + index * 4..12 + index * 4].copy_from_slice(&value.to_le_bytes());
    }
    Sid { words, length }
}

fn restricted_token(
    current: HANDLE,
    logon: &Sid,
    world: &Sid,
    write_sids: &[&Sid],
) -> io::Result<OwnedHandle> {
    let mut restricting = vec![
        SID_AND_ATTRIBUTES {
            Sid: logon.ptr(),
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: world.ptr(),
            Attributes: 0,
        },
    ];
    restricting.extend(write_sids.iter().map(|sid| SID_AND_ATTRIBUTES {
        Sid: sid.ptr(),
        Attributes: 0,
    }));
    let mut token = null_mut();
    if unsafe {
        CreateRestrictedToken(
            current,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
            0,
            null(),
            0,
            null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut token,
        )
    } == 0
    {
        return Err(last_error("CreateRestrictedToken"));
    }
    OwnedHandle::new(token, "CreateRestrictedToken")
}

fn set_default_dacl(token: HANDLE, sid: &Sid) -> io::Result<()> {
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TokenDefaultDacl, null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(last_error("GetTokenInformation(TokenDefaultDacl size)"));
    }
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            TokenDefaultDacl,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(last_error("GetTokenInformation(TokenDefaultDacl)"));
    }
    let old = unsafe { (*(buffer.as_ptr().cast::<TOKEN_DEFAULT_DACL>())).DefaultDacl };
    if old.is_null() {
        return Err(io::Error::other("restricted token has no default DACL"));
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.ptr().cast(),
        },
    };
    let mut new_acl = null_mut();
    let result = unsafe { SetEntriesInAclW(1, &entry, old, &mut new_acl) };
    if result != ERROR_SUCCESS || new_acl.is_null() {
        return Err(win32_error("SetEntriesInAclW(default DACL)", result));
    }
    let info = TOKEN_DEFAULT_DACL {
        DefaultDacl: new_acl,
    };
    let set = unsafe {
        SetTokenInformation(
            token,
            TokenDefaultDacl,
            std::ptr::from_ref(&info).cast(),
            size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    };
    let set_error = (set == 0).then(|| last_error("SetTokenInformation(TokenDefaultDacl)"));
    let free = unsafe { LocalFree(new_acl.cast()) };
    if let Some(error) = set_error {
        return Err(error);
    }
    if !free.is_null() {
        return Err(last_error("LocalFree(default DACL)"));
    }
    Ok(())
}

struct PrivateTemp {
    path: PathBuf,
    directory: PinnedDirectory,
    _marker: OwnedHandle,
}

impl PrivateTemp {
    fn cleanup(self, user_sid: &Sid) -> io::Result<()> {
        let path = self.path.clone();
        let _recovery = NamedMutex::acquire_recovery(user_sid)?;
        #[cfg(test)]
        pause_temp_cleanup();
        drop(self);
        if path.exists() {
            let directory = PinnedDirectory::open(&path, user_sid, true)?;
            let sid = capability_sid(path.as_os_str(), true);
            if acl_has_sid(directory.handle.0, &sid)? {
                revoke(&directory, &sid)?;
            }
            drop(directory);
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

fn create_private_temp(base: &Path, user_sid: &Sid) -> io::Result<PrivateTemp> {
    let _recovery = NamedMutex::acquire_recovery(user_sid)?;
    for _ in 0..64 {
        let path = base.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
        match fs::create_dir(&path) {
            Ok(()) => {
                let setup = (|| {
                    let marker_path = path.join(MARKER);
                    fs::write(
                        &marker_path,
                        format!("{MARKER_MAGIC}\n{}\n", hex(user_sid.bytes())),
                    )?;
                    #[cfg(test)]
                    pause_temp_publication();
                    let marker_w = wide(marker_path.as_os_str());
                    let marker = OwnedHandle::new(
                        unsafe {
                            CreateFileW(
                                marker_w.as_ptr(),
                                FILE_READ_DATA,
                                0,
                                null(),
                                OPEN_EXISTING,
                                FILE_FLAG_OPEN_REPARSE_POINT,
                                null_mut(),
                            )
                        },
                        "CreateFileW(temp marker)",
                    )?;
                    let directory = PinnedDirectory::open(&path, user_sid, true)?;
                    Ok(PrivateTemp {
                        path: path.clone(),
                        directory,
                        _marker: marker,
                    })
                })();
                match setup {
                    Ok(temp) => return Ok(temp),
                    Err(error) => {
                        let _ = fs::remove_dir_all(&path);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a private sandbox temp directory",
    ))
}

fn cleanup_stale(user_sid: &Sid) -> io::Result<()> {
    #[cfg(test)]
    stale_scan_entered();
    let _recovery = NamedMutex::acquire_recovery(user_sid)?;
    let base = fs::canonicalize(std::env::temp_dir())?;
    for entry in fs::read_dir(base)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(TEMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        let marker_path = path.join(MARKER);
        let marker_metadata = match fs::symlink_metadata(&marker_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !marker_metadata.is_file()
            || marker_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            continue;
        }
        let expected = format!("{MARKER_MAGIC}\n{}\n", hex(user_sid.bytes()));
        if !fs::read_to_string(&marker_path).is_ok_and(|contents| contents == expected) {
            continue;
        }
        let marker_w = wide(marker_path.as_os_str());
        let marker = match OwnedHandle::new(
            unsafe {
                CreateFileW(
                    marker_w.as_ptr(),
                    FILE_READ_DATA,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            },
            "CreateFileW(stale marker)",
        ) {
            Ok(marker) => marker,
            Err(_) => continue,
        };
        let directory = match PinnedDirectory::open(&path, user_sid, true) {
            Ok(directory) => directory,
            Err(_) => continue,
        };
        let sid = capability_sid(path.as_os_str(), true);
        if acl_has_sid(directory.handle.0, &sid)? {
            revoke(&directory, &sid)?;
        }
        drop(directory);
        drop(marker);
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn validate_volume(handle: HANDLE) -> io::Result<()> {
    let mut flags = 0u32;
    if unsafe {
        GetVolumeInformationByHandleW(
            handle,
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
    {
        return Err(last_error("GetVolumeInformationByHandleW"));
    }
    if flags & FILE_PERSISTENT_ACLS == 0 || flags & FILE_READ_ONLY_VOLUME != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "sandbox requires a writable volume with persistent ACLs",
        ));
    }
    Ok(())
}

fn final_path(handle: HANDLE) -> io::Result<PathBuf> {
    let needed = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            null_mut(),
            0,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if needed == 0 {
        return Err(last_error("GetFinalPathNameByHandleW(size)"));
    }
    let mut buffer = vec![0u16; needed as usize + 1];
    let written = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if written == 0 || written as usize >= buffer.len() {
        return Err(last_error("GetFinalPathNameByHandleW"));
    }
    Ok(PathBuf::from(OsString::from_wide(
        &buffer[..written as usize],
    )))
}

fn validate_bounded_argv(argv: &[String]) -> Result<(), TessivumError> {
    if argv.len() > MAX_ARGV
        || argv
            .iter()
            .any(|arg| arg.encode_utf16().count() > MAX_ARG_WCHARS)
    {
        Err(unavailable("target argv exceeds the Windows runner bound"))
    } else {
        Ok(())
    }
}

fn validate_native_argv(argv: &[String]) -> io::Result<()> {
    if argv.is_empty()
        || argv.len() > MAX_ARGV
        || argv[0].is_empty()
        || argv
            .iter()
            .any(|arg| arg.contains('\0') || arg.encode_utf16().count() > MAX_ARG_WCHARS)
    {
        Err(invalid("invalid or oversized target argv"))
    } else {
        Ok(())
    }
}

fn resolve_runner() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?.canonicalize().ok()?;
    if is_tessivum_exe(&current) {
        return Some(current);
    }
    let runner = [current.parent(), current.parent().and_then(Path::parent)]
        .into_iter()
        .flatten()
        .map(|parent| parent.join("tessivum.exe"))
        .find_map(|candidate| {
            candidate
                .is_file()
                .then(|| candidate.canonicalize().ok())
                .flatten()
        })
        .filter(|candidate| is_tessivum_exe(candidate));
    runner
}

fn is_tessivum_exe(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("tessivum.exe"))
}

pub(super) fn reject_reparse_ancestors(path: &Path) -> Result<(), TessivumError> {
    for ancestor in path.ancestors() {
        let Ok(metadata) = fs::symlink_metadata(ancestor) else {
            continue;
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(sandbox_error(
                "SANDBOX_INVALID_PATH",
                "sandbox paths cannot traverse reparse points",
                serde_json::json!({"path": path.display().to_string(), "reparse": ancestor.display().to_string()}),
            ));
        }
    }
    Ok(())
}

fn is_reparse(path: &Path) -> io::Result<bool> {
    Ok(fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn contains(root: &Path, candidate: &Path) -> bool {
    same_path(root, candidate) || candidate.starts_with(root)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn child_environment(temp: &Path) -> Vec<u16> {
    let temp = crate::process_path(temp);
    let mut entries: Vec<OsString> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !name.eq_ignore_ascii_case("TEMP") && !name.eq_ignore_ascii_case("TMP")
        })
        .map(|(mut name, value)| {
            name.push("=");
            name.push(value);
            name
        })
        .collect();
    for name in ["TEMP", "TMP"] {
        let mut entry = OsString::from(name);
        entry.push("=");
        entry.push(temp.as_os_str());
        entries.push(entry);
    }
    entries.sort_by_cached_key(|entry| entry.to_string_lossy().to_lowercase());

    let mut block = Vec::new();
    for entry in entries {
        block.extend(entry.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn build_command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| quote_argument(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            slashes += 1;
        } else {
            if character == '"' {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
            } else {
                quoted.push_str(&"\\".repeat(slashes));
            }
            quoted.push(character);
            slashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn drop_descriptor(descriptor: *mut c_void) -> io::Result<()> {
    if descriptor.is_null() || unsafe { LocalFree(descriptor) }.is_null() {
        Ok(())
    } else {
        Err(last_error("LocalFree(security descriptor)"))
    }
}

fn unavailable(detail: &str) -> TessivumError {
    sandbox_error(
        "SANDBOX_UNAVAILABLE",
        "requested Windows ACL confinement is unavailable",
        serde_json::json!({"detail": detail}),
    )
}

fn as_io(error: TessivumError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.message)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn last_error(operation: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

fn win32_error(operation: &str, code: u32) -> io::Error {
    io::Error::other(format!(
        "{operation}: {}",
        io::Error::from_raw_os_error(code as i32)
    ))
}

#[cfg(test)]
static FAIL_STAGE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(test)]
static FAIL_PROCESS_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
static TEMP_PUBLICATION_STAGE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(test)]
static STALE_SCAN_ENTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn pause_temp_publication() {
    use std::sync::atomic::Ordering;

    if TEMP_PUBLICATION_STAGE
        .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        while TEMP_PUBLICATION_STAGE.load(Ordering::SeqCst) == 2 {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
fn pause_temp_cleanup() {
    use std::sync::atomic::Ordering;

    if TEMP_PUBLICATION_STAGE
        .compare_exchange(4, 5, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        while TEMP_PUBLICATION_STAGE.load(Ordering::SeqCst) == 5 {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
fn stale_scan_entered() {
    STALE_SCAN_ENTERED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn injected(stage: u8) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_STAGE.load(std::sync::atomic::Ordering::SeqCst) == stage {
        return Err(io::Error::other(match stage {
            1 => "injected DACL refusal",
            2 => "injected launch refusal",
            3 => "injected pre-launch refusal",
            _ => "injected Job assignment refusal",
        }));
    }
    let _ = stage;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    fn runner_input(workspace: &Path, script: &str) -> RunnerInput {
        RunnerInput {
            version: PAYLOAD_VERSION,
            mode: SandboxMode::WorkspaceWrite,
            workspace: workspace.to_path_buf(),
            write_roots: vec![workspace.to_path_buf()],
            argv: vec![
                "powershell.exe".into(),
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool, message: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !condition() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(condition(), "{message}");
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, WaitForSingleObject},
        };

        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 0x0000_0102;
        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return false;
        }
        let alive = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
        unsafe { CloseHandle(process) };
        alive
    }

    #[test]
    fn root_capability_is_stable_distinct_and_separate_from_temp() {
        let first_path = OsStr::new(r"C:\work\中文");
        let second_path = OsStr::new(r"C:\work\other");
        let first = capability_sid(first_path, false);
        let repeated = capability_sid(first_path, false);
        let second = capability_sid(second_path, false);
        let temp = capability_sid(first_path, true);
        assert_eq!(first.bytes(), repeated.bytes());
        assert_ne!(first.bytes(), second.bytes());
        assert_ne!(first.bytes(), temp.bytes());
    }

    #[test]
    fn command_line_quoting_preserves_empty_quotes_and_trailing_slashes() {
        assert_eq!(quote_argument(""), "\"\"");
        assert_eq!(quote_argument("a b\\"), "\"a b\\\\\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn temp_publication_and_deletion_exclude_stale_recovery() {
        let _lock = TEST_LOCK.lock();
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let (path_tx, path_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        TEMP_PUBLICATION_STAGE.store(1, std::sync::atomic::Ordering::SeqCst);
        STALE_SCAN_ENTERED.store(false, std::sync::atomic::Ordering::SeqCst);

        let creator = std::thread::spawn(move || {
            let token = open_current_token().unwrap();
            let user_sid = token_sid(token.0, TokenUser, false).unwrap();
            let temp = create_private_temp(&base, &user_sid).unwrap();
            path_tx.send(temp.path.clone()).unwrap();
            release_rx.recv().unwrap();
            temp.cleanup(&user_sid).unwrap();
        });
        wait_until(
            || TEMP_PUBLICATION_STAGE.load(std::sync::atomic::Ordering::SeqCst) == 2,
            "temp allocation did not pause after marker publication",
        );

        let collector = std::thread::spawn(|| {
            let token = open_current_token().unwrap();
            let user_sid = token_sid(token.0, TokenUser, false).unwrap();
            cleanup_stale(&user_sid).unwrap();
        });
        wait_until(
            || STALE_SCAN_ENTERED.load(std::sync::atomic::Ordering::SeqCst),
            "stale collector did not attempt recovery",
        );
        TEMP_PUBLICATION_STAGE.store(3, std::sync::atomic::Ordering::SeqCst);

        let path = path_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        collector.join().unwrap();
        assert!(path.exists(), "stale recovery deleted a live allocation");

        TEMP_PUBLICATION_STAGE.store(4, std::sync::atomic::Ordering::SeqCst);
        STALE_SCAN_ENTERED.store(false, std::sync::atomic::Ordering::SeqCst);
        release_tx.send(()).unwrap();
        wait_until(
            || TEMP_PUBLICATION_STAGE.load(std::sync::atomic::Ordering::SeqCst) == 5,
            "normal cleanup did not pause before releasing its live marker",
        );
        let final_collector = std::thread::spawn(|| {
            let token = open_current_token().unwrap();
            let user_sid = token_sid(token.0, TokenUser, false).unwrap();
            cleanup_stale(&user_sid).unwrap();
        });
        wait_until(
            || STALE_SCAN_ENTERED.load(std::sync::atomic::Ordering::SeqCst),
            "stale collector did not contend with final cleanup",
        );
        TEMP_PUBLICATION_STAGE.store(6, std::sync::atomic::Ordering::SeqCst);
        creator.join().unwrap();
        final_collector.join().unwrap();
        assert!(!path.exists(), "normal cleanup did not remove private temp");
        TEMP_PUBLICATION_STAGE.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn injected_failures_reach_their_stage_without_mutating_environment() {
        let _lock = TEST_LOCK.lock();
        let workspace = std::env::temp_dir().join(format!(
            "tessivum-acl-failure-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let marker = workspace.join("must-not-run.txt");
        let script = format!(
            "Set-Content -LiteralPath '{}' -Value ran",
            marker.display().to_string().replace('\'', "''")
        );
        let temp_before = std::env::var_os("TEMP");
        let tmp_before = std::env::var_os("TMP");

        FAIL_STAGE.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(run(runner_input(&workspace, &script)).unwrap(), 0);
        assert!(
            marker.exists(),
            "known-good launch did not reach the target"
        );
        fs::remove_file(&marker).unwrap();
        assert_eq!(std::env::var_os("TEMP"), temp_before);
        assert_eq!(std::env::var_os("TMP"), tmp_before);

        for (stage, expected) in [
            (1, "injected DACL refusal"),
            (2, "injected launch refusal"),
            (3, "injected pre-launch refusal"),
            (4, "injected Job assignment refusal"),
        ] {
            FAIL_PROCESS_ID.store(0, std::sync::atomic::Ordering::SeqCst);
            FAIL_STAGE.store(stage, std::sync::atomic::Ordering::SeqCst);
            let error = run(runner_input(&workspace, &script)).unwrap_err();
            assert_eq!(error.to_string(), expected, "failure stage {stage}");
            assert!(!marker.exists(), "failure stage {stage} ran the target");
            assert_eq!(std::env::var_os("TEMP"), temp_before);
            assert_eq!(std::env::var_os("TMP"), tmp_before);

            let pid = FAIL_PROCESS_ID.load(std::sync::atomic::Ordering::SeqCst);
            if stage == 4 {
                assert_ne!(pid, 0, "Job refusal did not create a suspended child");
                assert!(
                    !process_is_alive(pid),
                    "Job refusal did not reap child {pid}"
                );
            } else {
                assert_eq!(pid, 0, "failure stage {stage} created a child early");
            }
        }
        FAIL_STAGE.store(0, std::sync::atomic::Ordering::SeqCst);
        fs::remove_dir_all(workspace).unwrap();
    }
}
