//! Content-addressed v1 image attachments confined to one durable root.

use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use tessivum_core::ServiceKey;
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};
use uuid::Uuid;

pub fn attachments_service_key() -> ServiceKey {
    ServiceKey::new("harness.attachments", "1")
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageMediaType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/webp")]
    Webp,
    #[serde(rename = "image/gif")]
    Gif,
}
impl ImageMediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }
}

/// A sha256 identity is opaque to callers: it is accepted only in its exact
/// generated form and is never used as a path component verbatim.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AttachmentId(String);
impl AttachmentId {
    fn from_digest(digest: [u8; 32]) -> Self {
        Self(format!("sha256:{}", hex(&digest)))
    }
    fn digest_hex(&self) -> Result<&str, AttachmentError> {
        let Some(digest) = self.0.strip_prefix("sha256:") else {
            return Err(AttachmentError::InvalidId);
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(AttachmentError::InvalidId);
        }
        Ok(digest)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AttachmentId").field(&self.0).finish()
    }
}
impl<'de> Deserialize<'de> for AttachmentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        AttachmentId::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl TryFrom<String> for AttachmentId {
    type Error = AttachmentError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let id = Self(value);
        id.digest_hex()?;
        Ok(id)
    }
}
impl TryFrom<&str> for AttachmentId {
    type Error = AttachmentError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_owned())
    }
}

/// This serializable reference contains no filesystem path or encoded bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentRef {
    pub attachment_id: AttachmentId,
    pub media_type: ImageMediaType,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

pub struct AttachmentInput {
    pub data: Vec<u8>,
    pub name: Option<String>,
}
impl AttachmentInput {
    pub fn new(data: Vec<u8>, name: Option<String>) -> Self {
        Self { data, name }
    }
}
impl fmt::Debug for AttachmentInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentInput")
            .field("byte_len", &self.data.len())
            .field("has_name", &self.name.is_some())
            .finish()
    }
}

pub struct AttachmentData {
    pub reference: AttachmentRef,
    pub data: Vec<u8>,
}
impl fmt::Debug for AttachmentData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentData")
            .field("reference", &self.reference)
            .field("byte_len", &self.data.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct AttachmentLimits {
    pub max_image_bytes: u64,
    pub max_images_per_message: usize,
    pub max_message_image_bytes: u64,
    pub max_image_pixels: u64,
    pub media_types: BTreeSet<ImageMediaType>,
}
impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: 20 * 1024 * 1024,
            max_images_per_message: 16,
            max_message_image_bytes: 40 * 1024 * 1024,
            max_image_pixels: 40_000_000,
            media_types: [
                ImageMediaType::Png,
                ImageMediaType::Jpeg,
                ImageMediaType::Webp,
                ImageMediaType::Gif,
            ]
            .into_iter()
            .collect(),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum AttachmentError {
    #[error("attachment id is invalid")]
    InvalidId,
    #[error("attachment is not an admitted image type")]
    UnsupportedMediaType,
    #[error("attachment image header is invalid")]
    InvalidImage,
    #[error("attachment image exceeds configured byte limit")]
    ByteLimit,
    #[error("attachment image exceeds configured pixel limit")]
    PixelLimit,
    #[error("attachment batch exceeds configured image count")]
    BatchCountLimit,
    #[error("attachment batch exceeds configured byte limit")]
    BatchByteLimit,
    #[error("attachment storage failed: {0}")]
    Storage(String),
    #[error("attachment digest verification failed")]
    DigestMismatch,
    #[error("attachment metadata verification failed")]
    MetadataMismatch,
}
impl AttachmentError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidId => "INVALID_ATTACHMENT_ID",
            Self::UnsupportedMediaType => "UNSUPPORTED_ATTACHMENT_MEDIA_TYPE",
            Self::InvalidImage => "INVALID_ATTACHMENT_IMAGE",
            Self::ByteLimit => "ATTACHMENT_BYTE_LIMIT",
            Self::PixelLimit => "ATTACHMENT_PIXEL_LIMIT",
            Self::BatchCountLimit => "ATTACHMENT_BATCH_COUNT_LIMIT",
            Self::BatchByteLimit => "ATTACHMENT_BATCH_BYTE_LIMIT",
            Self::Storage(_) => "ATTACHMENT_STORAGE_FAILED",
            Self::DigestMismatch => "ATTACHMENT_DIGEST_MISMATCH",
            Self::MetadataMismatch => "ATTACHMENT_METADATA_MISMATCH",
        }
    }
}

struct ValidatedImage {
    media_type: ImageMediaType,
    bytes: u64,
    width: u32,
    height: u32,
    id: AttachmentId,
    name: Option<String>,
}

/// The `v1` subdirectory and generated digest names are the complete storage
/// layout. No caller-controlled string can select a path below `root`.
#[derive(Clone, Debug)]
pub struct AttachmentStore {
    root: PathBuf,
    limits: AttachmentLimits,
}
impl AttachmentStore {
    pub fn new(
        root: impl Into<PathBuf>,
        limits: AttachmentLimits,
    ) -> Result<Self, AttachmentError> {
        if limits.max_image_bytes == 0
            || limits.max_images_per_message == 0
            || limits.max_message_image_bytes == 0
            || limits.max_image_pixels == 0
            || limits.media_types.is_empty()
        {
            return Err(AttachmentError::Storage(
                "attachment limits must be positive and admit a media type".into(),
            ));
        }
        Ok(Self {
            root: root.into(),
            limits,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn limits(&self) -> &AttachmentLimits {
        &self.limits
    }
    pub fn validate(&self, input: &AttachmentInput) -> Result<AttachmentRef, AttachmentError> {
        Ok(self
            .validate_bytes(&input.data, input.name.as_deref())?
            .reference())
    }
    pub async fn save(&self, input: AttachmentInput) -> Result<AttachmentRef, AttachmentError> {
        let image = self.validate_bytes(&input.data, input.name.as_deref())?;
        Ok(self.persist(&input.data, image).await?.0)
    }
    /// Performs every byte, media, dimension, pixel, and total-size check before
    /// creating any file. A later IO failure removes only files this batch made.
    pub async fn save_batch(
        &self,
        inputs: Vec<AttachmentInput>,
    ) -> Result<Vec<AttachmentRef>, AttachmentError> {
        if inputs.len() > self.limits.max_images_per_message {
            return Err(AttachmentError::BatchCountLimit);
        }
        let mut total = 0u64;
        let mut validated = Vec::with_capacity(inputs.len());
        for input in &inputs {
            let image = self.validate_bytes(&input.data, input.name.as_deref())?;
            total = total
                .checked_add(image.bytes)
                .ok_or(AttachmentError::BatchByteLimit)?;
            if total > self.limits.max_message_image_bytes {
                return Err(AttachmentError::BatchByteLimit);
            }
            validated.push(image);
        }
        let mut saved = Vec::with_capacity(inputs.len());
        let mut created = Vec::new();
        for (input, image) in inputs.iter().zip(validated) {
            match self.persist(&input.data, image).await {
                Ok((reference, was_created)) => {
                    if was_created {
                        created.push(reference.attachment_id.clone());
                    }
                    saved.push(reference);
                }
                Err(error) => {
                    for id in created {
                        let _ = self.remove_if_confined(&id).await;
                    }
                    return Err(error);
                }
            }
        }
        Ok(saved)
    }
    pub async fn read(&self, id: &AttachmentId) -> Result<AttachmentData, AttachmentError> {
        let directory = self.directory().await?;
        let digest = id.digest_hex()?;
        let target = directory.join(digest);
        let canonical = fs::canonicalize(&target)
            .await
            .map_err(|error| AttachmentError::Storage(format!("open attachment: {error}")))?;
        if !canonical.starts_with(&directory) {
            return Err(AttachmentError::Storage(
                "attachment path escapes configured root".into(),
            ));
        }
        let data = fs::read(&canonical)
            .await
            .map_err(|error| AttachmentError::Storage(format!("read attachment: {error}")))?;
        let image = self.validate_bytes(&data, None)?;
        if &image.id != id {
            return Err(AttachmentError::DigestMismatch);
        }
        Ok(AttachmentData {
            reference: image.reference(),
            data,
        })
    }
    /// Verifies supplied media metadata against bytes instead of trusting a ref.
    pub async fn read_ref(&self, reference: &AttachmentRef) -> Result<Vec<u8>, AttachmentError> {
        let read = self.read(&reference.attachment_id).await?;
        if read.reference.media_type != reference.media_type
            || read.reference.bytes != reference.bytes
            || read.reference.width != reference.width
            || read.reference.height != reference.height
        {
            return Err(AttachmentError::MetadataMismatch);
        }
        Ok(read.data)
    }

    fn validate_bytes(
        &self,
        data: &[u8],
        name: Option<&str>,
    ) -> Result<ValidatedImage, AttachmentError> {
        let bytes = u64::try_from(data.len()).map_err(|_| AttachmentError::ByteLimit)?;
        if bytes > self.limits.max_image_bytes {
            return Err(AttachmentError::ByteLimit);
        }
        let (media_type, width, height) = parse_image(data)?;
        if !self.limits.media_types.contains(&media_type) {
            return Err(AttachmentError::UnsupportedMediaType);
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(AttachmentError::PixelLimit)?;
        if width == 0 || height == 0 || pixels > self.limits.max_image_pixels {
            return Err(AttachmentError::PixelLimit);
        }
        let digest: [u8; 32] = Sha256::digest(data).into();
        Ok(ValidatedImage {
            media_type,
            bytes,
            width,
            height,
            id: AttachmentId::from_digest(digest),
            name: sanitize_name(name),
        })
    }
    async fn directory(&self) -> Result<PathBuf, AttachmentError> {
        let directory = self.root.join("v1");
        fs::create_dir_all(&directory).await.map_err(|error| {
            AttachmentError::Storage(format!("create attachment root: {error}"))
        })?;
        let canonical = fs::canonicalize(&directory).await.map_err(|error| {
            AttachmentError::Storage(format!("canonicalize attachment root: {error}"))
        })?;
        if !canonical.is_dir() {
            return Err(AttachmentError::Storage(
                "attachment root is not a directory".into(),
            ));
        }
        Ok(canonical)
    }
    async fn persist(
        &self,
        data: &[u8],
        image: ValidatedImage,
    ) -> Result<(AttachmentRef, bool), AttachmentError> {
        let directory = self.directory().await?;
        let target = directory.join(image.id.digest_hex()?);
        match fs::canonicalize(&target).await {
            Ok(existing) => {
                if !existing.starts_with(&directory) {
                    return Err(AttachmentError::Storage(
                        "attachment path escapes configured root".into(),
                    ));
                }
                let existing_data = fs::read(existing).await.map_err(|error| {
                    AttachmentError::Storage(format!("read existing attachment: {error}"))
                })?;
                let verified = self.validate_bytes(&existing_data, image.name.as_deref())?;
                if verified.id != image.id {
                    return Err(AttachmentError::DigestMismatch);
                }
                return Ok((image.reference(), false));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AttachmentError::Storage(format!(
                    "inspect attachment: {error}"
                )))
            }
        }
        let temporary = directory.join(format!(
            ".{}-{}.tmp",
            image.id.digest_hex()?,
            Uuid::new_v4()
        ));
        let result = async {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await.map_err(|error| {
                AttachmentError::Storage(format!("create attachment temporary: {error}"))
            })?;
            file.write_all(data).await.map_err(|error| {
                AttachmentError::Storage(format!("write attachment temporary: {error}"))
            })?;
            file.sync_all().await.map_err(|error| {
                AttachmentError::Storage(format!("sync attachment temporary: {error}"))
            })?;
            fs::rename(&temporary, &target).await.map_err(|error| {
                AttachmentError::Storage(format!("rename attachment temporary: {error}"))
            })
        }
        .await;
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary).await;
            return Err(error);
        }
        Ok((image.reference(), true))
    }
    async fn remove_if_confined(&self, id: &AttachmentId) -> Result<(), AttachmentError> {
        let directory = self.directory().await?;
        let target = directory.join(id.digest_hex()?);
        match fs::canonicalize(&target).await {
            Ok(path) if path.starts_with(&directory) => fs::remove_file(path)
                .await
                .map_err(|error| AttachmentError::Storage(format!("remove attachment: {error}"))),
            Ok(_) => Err(AttachmentError::Storage(
                "attachment path escapes configured root".into(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AttachmentError::Storage(format!(
                "inspect attachment: {error}"
            ))),
        }
    }
}
impl ValidatedImage {
    fn reference(&self) -> AttachmentRef {
        AttachmentRef {
            attachment_id: self.id.clone(),
            media_type: self.media_type,
            bytes: self.bytes,
            width: self.width,
            height: self.height,
            name: self.name.clone(),
        }
    }
}

fn parse_image(data: &[u8]) -> Result<(ImageMediaType, u32, u32), AttachmentError> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        if data.len() < 24 || &data[12..16] != b"IHDR" || be32(&data[8..12]) != 13 {
            return Err(AttachmentError::InvalidImage);
        }
        return Ok((
            ImageMediaType::Png,
            be32(&data[16..20]),
            be32(&data[20..24]),
        ));
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        if data.len() < 10 {
            return Err(AttachmentError::InvalidImage);
        }
        return Ok((
            ImageMediaType::Gif,
            le16(&data[6..8]).into(),
            le16(&data[8..10]).into(),
        ));
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return parse_webp(data);
    }
    if data.starts_with(b"\xff\xd8") {
        return parse_jpeg(data);
    }
    Err(AttachmentError::UnsupportedMediaType)
}
fn parse_jpeg(data: &[u8]) -> Result<(ImageMediaType, u32, u32), AttachmentError> {
    let mut offset = 2;
    while offset + 4 <= data.len() {
        if data[offset] != 0xff {
            return Err(AttachmentError::InvalidImage);
        }
        while offset < data.len() && data[offset] == 0xff {
            offset += 1;
        }
        if offset >= data.len() {
            break;
        }
        let marker = data[offset];
        offset += 1;
        if marker == 0xd8 || marker == 0xd9 {
            continue;
        }
        if offset + 2 > data.len() {
            break;
        }
        let length = usize::from(be16(&data[offset..offset + 2]));
        if length < 2 || offset + length > data.len() {
            return Err(AttachmentError::InvalidImage);
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8 {
                return Err(AttachmentError::InvalidImage);
            }
            return Ok((
                ImageMediaType::Jpeg,
                u32::from(be16(&data[offset + 5..offset + 7])),
                u32::from(be16(&data[offset + 3..offset + 5])),
            ));
        }
        offset += length;
    }
    Err(AttachmentError::InvalidImage)
}
fn parse_webp(data: &[u8]) -> Result<(ImageMediaType, u32, u32), AttachmentError> {
    if data.len() < 30 {
        return Err(AttachmentError::InvalidImage);
    }
    match &data[12..16] {
        b"VP8X" if data.len() >= 30 => Ok((
            ImageMediaType::Webp,
            le24(&data[24..27]) + 1,
            le24(&data[27..30]) + 1,
        )),
        b"VP8 " if data.len() >= 30 && &data[23..26] == b"\x9d\x01\x2a" => Ok((
            ImageMediaType::Webp,
            u32::from(le16(&data[26..28]) & 0x3fff),
            u32::from(le16(&data[28..30]) & 0x3fff),
        )),
        b"VP8L" if data.len() >= 25 && data[20] == 0x2f => {
            let bits = u32::from_le_bytes([data[21], data[22], data[23], data[24]]);
            Ok((
                ImageMediaType::Webp,
                (bits & 0x3fff) + 1,
                ((bits >> 14) & 0x3fff) + 1,
            ))
        }
        _ => Err(AttachmentError::InvalidImage),
    }
}
fn sanitize_name(name: Option<&str>) -> Option<String> {
    let name = name?.trim();
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        None
    } else {
        Some(name.to_owned())
    }
}
fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}
fn le24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}
fn hex(digest: &[u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}
