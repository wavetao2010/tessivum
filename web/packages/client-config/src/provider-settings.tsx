import * as React from 'react';
import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react';
import type { Context } from '@deepseek-ai/cordis';
import type { IApiClient } from '@deepseek-ai/dsh-client-connection/client';
import type {} from '@deepseek-ai/dsh-client-runtime/client';
import type {} from '@deepseek-ai/dsh-client-ui-settings/client';

const PROVIDER_SETTINGS_NS = 'llm-openai-responses';
const DEFAULT_MODEL_SETTINGS_NS = 'agent-default-model';
const PROVIDER_API = 'openai-responses';
const MAX_CREDENTIAL_REFS = 64;

type Modality = 'text' | 'image';

export interface ProviderModelDraft {
  id: string;
  name: string;
  input: Modality[];
  contextWindow: string;
  maxTokens: string;
}

export interface ProviderDraft {
  id: string;
  displayName: string;
  baseURL: string;
  apiKeyEnv: string;
  models: ProviderModelDraft[];
}

interface ProviderModel {
  id: string;
  name?: string;
  input: Modality[];
  contextWindow?: number;
  maxTokens?: number;
}

interface ProviderProfile {
  displayName: string;
  baseURL: string;
  apiKeyEnv: string;
  models: ProviderModel[];
}

interface SettingsView {
  ns: string;
  value: unknown;
  revision: number;
}

interface ProviderDirectoryEntry {
  provider: string;
  displayName: string;
  settingsNs: string;
  settingsPath: string[];
  active: boolean;
  declared?: boolean;
  credentialConfigured?: boolean;
}

interface CatalogModel {
  id: string;
  name: string;
  inputModalities?: Modality[];
  contextWindow?: number;
  maxTokens?: number;
  routable?: boolean;
}

interface CatalogFailure {
  id: string;
  name: string;
  message: string;
}

interface CatalogGroup {
  id: string;
  name: string;
  models: CatalogModel[];
  credentialConfigured?: boolean;
  routable?: boolean;
  failure?: CatalogFailure;
}

interface CredentialDescriptor {
  configured: boolean;
  writable: boolean;
  source?: string;
}

interface DiscoveredModel {
  id: string;
  name?: string;
  contextWindow?: number;
  maxTokens?: number;
}

export interface DefaultSelection {
  provider: string;
  model: string;
}

export interface ProviderCredentialProfile {
  apiKeyEnv: string;
}

export interface DraftConflictInput {
  dirty: boolean;
  observedRevision: number;
  remoteRevision: number;
  observedFingerprint: string;
  remoteFingerprint: string;
}

export type ProviderDeleteDefaultDecision =
  | { kind: 'keep' }
  | { kind: 'replace'; selection: DefaultSelection; notice: string }
  | { kind: 'block'; notice: string };

interface ProviderSettingsSnapshot {
  writable: boolean;
  providerSettings: SettingsView;
  defaultSettings: SettingsView;
  profiles: Record<string, ProviderProfile>;
  providers: ProviderDirectoryEntry[];
  groups: CatalogGroup[];
  failures: CatalogFailure[];
  credentials: Record<string, CredentialDescriptor>;
}

type RpcResponse<T> = {
  result:
    | { ok: true; value: T }
    | { ok: false; error: { code: string; message: string } };
};

export function deriveCredentialRef(providerId: string): string {
  return `${providerId.toUpperCase().replace(/[^A-Z0-9]/g, '_')}_API_KEY`;
}

const PROVIDER_ID_PATTERN = /^[a-z][a-z0-9-]*$/;

export function hasDraftConflict(input: DraftConflictInput): boolean {
  return input.dirty && (
    input.observedRevision !== input.remoteRevision
    || input.observedFingerprint !== input.remoteFingerprint
  );
}

export function discoveryFingerprint(
  draft: Pick<ProviderDraft, 'id' | 'baseURL'>,
  apiKey: string,
): string {
  return JSON.stringify([draft.id.trim(), draft.baseURL.trim(), apiKey]);
}

export function isCurrentDiscoveryResult(
  expectedFingerprint: string,
  currentFingerprint: string,
  expectedGeneration: number,
  currentGeneration: number,
): boolean {
  return expectedGeneration === currentGeneration && expectedFingerprint === currentFingerprint;
}

export function decideDefaultAfterProviderDelete(
  providerId: string,
  selection: DefaultSelection | undefined,
  groups: readonly CatalogGroup[],
): ProviderDeleteDefaultDecision {
  if (selection?.provider !== providerId) return { kind: 'keep' };
  const group = groups.find((candidate) => (
    candidate.id !== providerId
    && candidate.routable === true
    && candidate.models.some((model) => model.routable === true)
  ));
  const model = group?.models.find((candidate) => candidate.routable === true);
  if (group && model) {
    return {
      kind: 'replace',
      selection: { provider: group.id, model: model.id },
      notice: `The default was changed to “${model.name || model.id}” from “${group.name || group.id}”.`,
    };
  }
  return {
    kind: 'block',
    notice: 'This provider is the current default, and no other routable model is available. Configure another routable provider and model, then try again.',
  };
}

export function validateProviderIdentity(
  draft: Pick<ProviderDraft, 'id' | 'apiKeyEnv'>,
  profiles: Readonly<Record<string, ProviderCredentialProfile>>,
  options: {
    isNew: boolean;
    originalId?: string;
    persistedCredentialRef?: string;
  },
): string[] {
  const id = draft.id.trim();
  const errors: string[] = [];
  if (id && !PROVIDER_ID_PATTERN.test(id)) {
    errors.push('Provider ID must start with a lowercase letter and contain only lowercase letters, digits, and hyphens.');
  }
  if (options.originalId !== undefined && id !== options.originalId) {
    errors.push('A saved provider ID cannot be changed.');
  }
  if (options.persistedCredentialRef !== undefined && draft.apiKeyEnv !== options.persistedCredentialRef) {
    errors.push('The saved provider credential reference cannot be changed.');
  }
  const duplicate = id && Object.keys(profiles).some((savedId) => savedId === id && savedId !== options.originalId);
  if (duplicate) errors.push('A provider with this ID already exists.');

  if (options.isNew && id) {
    const derived = deriveCredentialRef(id);
    if (draft.apiKeyEnv !== derived) {
      errors.push('New provider credential references must match the provider ID.');
    }
    const alias = Object.entries(profiles).some(([savedId, profile]) => (
      savedId !== options.originalId && profile.apiKeyEnv === derived
    ));
    if (alias) errors.push('This provider ID would reuse a credential reference owned by another provider.');
  }
  return errors;
}

export function validateApiKey(value: string): string | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  if (value !== trimmed) return 'API key cannot contain leading or trailing whitespace.';
  if (!/^[\x21-\x7E]+$/.test(value)) return 'API key must contain printable ASCII characters only.';
  if (/^[A-Za-z_][A-Za-z0-9_]*=.*$/.test(value)) return 'Paste only the API key, without a NAME= prefix.';
  const quote = value[0];
  return (quote === '"' || quote === "'") && value.at(-1) === quote
    ? 'Paste the API key without surrounding quotes.'
    : undefined;
}

function baseUrlError(value: string): string | undefined {
  const source = value.trim();
  if (!source) return 'Base URL is required.';
  try {
    const url = new URL(source);
    if (
      (url.protocol !== 'http:' && url.protocol !== 'https:')
      || !url.hostname
      || url.username
      || url.password
      || source.includes('?')
      || source.includes('#')
    ) {
      return 'Base URL must be HTTP(S) and cannot include credentials, a query, or a fragment.';
    }
  } catch {
    return 'Base URL must be a valid HTTP(S) URL.';
  }
  return undefined;
}

function capacityError(value: string, label: string): string | undefined {
  if (!value.trim()) return undefined;
  const number = Number(value);
  return Number.isSafeInteger(number) && number > 0
    ? undefined
    : `${label} must be a positive safe integer.`;
}

export function validateProviderDraft(draft: ProviderDraft): string[] {
  const errors: string[] = [];
  const providerId = draft.id.trim();
  if (!providerId) errors.push('Provider ID is required.');
  else if (!PROVIDER_ID_PATTERN.test(providerId)) {
    errors.push('Provider ID must start with a lowercase letter and contain only lowercase letters, digits, and hyphens.');
  }
  if (providerId && !/^[A-Z_][A-Z0-9_]*$/.test(draft.apiKeyEnv)) {
    errors.push('Provider ID must derive a valid credential reference (begin with a letter or underscore).');
  }
  if (!draft.displayName.trim()) errors.push('Display name is required.');
  const urlError = baseUrlError(draft.baseURL);
  if (urlError) errors.push(urlError);
  if (draft.models.length === 0) errors.push('Add at least one model.');

  const modelIds = new Set<string>();
  draft.models.forEach((model, index) => {
    const label = `Model ${index + 1}`;
    const id = model.id.trim();
    if (!id) errors.push(`${label} ID is required.`);
    else if (modelIds.has(id)) errors.push(`Model ID “${id}” is duplicated.`);
    else modelIds.add(id);
    if (model.input.length === 0) errors.push(`${label} must accept text or image input.`);
    const contextError = capacityError(model.contextWindow, `${label} context window`);
    if (contextError) errors.push(contextError);
    const outputError = capacityError(model.maxTokens, `${label} max output tokens`);
    if (outputError) errors.push(outputError);
  });
  return errors;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function readProfiles(value: unknown): Record<string, ProviderProfile> {
  if (!isRecord(value) || !isRecord(value.providers)) return {};
  const profiles: Record<string, ProviderProfile> = {};
  for (const [id, candidate] of Object.entries(value.providers)) {
    if (!isRecord(candidate) || !Array.isArray(candidate.models)) continue;
    const models: ProviderModel[] = candidate.models.flatMap((model) => {
      if (!isRecord(model) || typeof model.id !== 'string') return [];
      const input = Array.isArray(model.input)
        ? model.input.filter((item): item is Modality => item === 'text' || item === 'image')
        : ['text' as const];
      return [{
        id: model.id,
        name: typeof model.name === 'string' ? model.name : undefined,
        input: input.length ? input : ['text'],
        contextWindow: typeof model.contextWindow === 'number' ? model.contextWindow : undefined,
        maxTokens: typeof model.maxTokens === 'number' ? model.maxTokens : undefined,
      }];
    });
    if (
      typeof candidate.displayName === 'string'
      && typeof candidate.baseURL === 'string'
      && typeof candidate.apiKeyEnv === 'string'
    ) {
      profiles[id] = {
        displayName: candidate.displayName,
        baseURL: candidate.baseURL,
        apiKeyEnv: candidate.apiKeyEnv,
        models,
      };
    }
  }
  return profiles;
}

function readDefaultSelection(value: unknown): DefaultSelection | undefined {
  if (!isRecord(value) || typeof value.provider !== 'string' || typeof value.model !== 'string') {
    return undefined;
  }
  return { provider: value.provider, model: value.model };
}

function unwrap<T>(response: RpcResponse<T>): T {
  if (!response.result.ok) {
    throw new Error(response.result.error.message || response.result.error.code);
  }
  return response.result.value;
}

function publicError(error: unknown, secrets: string[] = []): string {
  let message = error instanceof Error ? error.message : 'The operation failed.';
  for (const secret of secrets) {
    if (secret) message = message.replaceAll(secret, '[redacted]');
  }
  return message || 'The operation failed.';
}

function profileToDraft(id: string, profile: ProviderProfile): ProviderDraft {
  return {
    id,
    displayName: profile.displayName,
    baseURL: profile.baseURL,
    apiKeyEnv: profile.apiKeyEnv,
    models: profile.models.map((model) => ({
      id: model.id,
      name: model.name ?? '',
      input: [...model.input],
      contextWindow: model.contextWindow === undefined ? '' : String(model.contextWindow),
      maxTokens: model.maxTokens === undefined ? '' : String(model.maxTokens),
    })),
  };
}

const NEW_PROVIDER_PROFILE: ProviderProfile = {
  displayName: '',
  baseURL: '',
  apiKeyEnv: deriveCredentialRef(''),
  models: [{ id: '', input: ['text'] }],
};

function profileFromDraft(draft: ProviderDraft): ProviderProfile {
  return {
    displayName: draft.displayName.trim(),
    baseURL: draft.baseURL.trim(),
    apiKeyEnv: draft.apiKeyEnv,
    models: draft.models.map((model) => {
      const name = model.name.trim();
      const contextWindow = model.contextWindow.trim();
      const maxTokens = model.maxTokens.trim();
      return {
        id: model.id.trim(),
        ...(name ? { name } : {}),
        input: [...model.input],
        ...(contextWindow ? { contextWindow: Number(contextWindow) } : {}),
        ...(maxTokens ? { maxTokens: Number(maxTokens) } : {}),
      };
    }),
  };
}

function settingsFingerprint(draft: ProviderDraft): string {
  return JSON.stringify(draft);
}

function selectionKey(selection: DefaultSelection | undefined): string {
  return selection ? JSON.stringify(selection) : '';
}

export function normalizeCatalogReadiness(
  groups: readonly CatalogGroup[],
  profiles: Readonly<Record<string, ProviderProfile>>,
  providers: readonly ProviderDirectoryEntry[],
  credentials: Readonly<Record<string, CredentialDescriptor>>,
): CatalogGroup[] {
  return groups.map((group) => {
    const profile = profiles[group.id];
    const configured = profile !== undefined && credentials[profile.apiKeyEnv]?.configured === true;
    const routable = configured
      && providers.some((provider) => provider.provider === group.id && provider.active === true);
    return {
      ...group,
      credentialConfigured: configured,
      routable,
      models: group.models.map((model) => ({ ...model, routable })),
    };
  });
}

async function loadSnapshot(api: IApiClient): Promise<ProviderSettingsSnapshot> {
  const [settingsResponse, providersResponse, modelsResponse] = await Promise.all([
    api.settings.describe({}),
    api.llm.providers({}),
    api.llm.models({}),
  ]);
  const settings = unwrap(settingsResponse as RpcResponse<{
    writable: boolean;
    namespaces: SettingsView[];
  }>);
  const providerSettings = settings.namespaces.find((item) => item.ns === PROVIDER_SETTINGS_NS);
  const defaultSettings = settings.namespaces.find((item) => item.ns === DEFAULT_MODEL_SETTINGS_NS);
  if (!providerSettings || !defaultSettings) {
    throw new Error('Provider settings are not available from this host.');
  }
  const profiles = readProfiles(providerSettings.value);
  const providerData = unwrap(providersResponse as RpcResponse<{ providers: ProviderDirectoryEntry[] }>);
  const modelData = unwrap(modelsResponse as unknown as RpcResponse<{
    groups: CatalogGroup[];
    failures: CatalogFailure[];
  }>);
  const refs = [...new Set(Object.values(profiles).map((profile) => profile.apiKeyEnv))];
  const credentialBatches = await Promise.all(
    Array.from({ length: Math.ceil(refs.length / MAX_CREDENTIAL_REFS) }, (_, index) =>
      api.credentials.describe({
        refs: refs.slice(index * MAX_CREDENTIAL_REFS, (index + 1) * MAX_CREDENTIAL_REFS),
      })),
  );
  const credentials: Record<string, CredentialDescriptor> = {};
  for (const response of credentialBatches) {
    Object.assign(
      credentials,
      unwrap(response as RpcResponse<{ credentials: Record<string, CredentialDescriptor> }>).credentials,
    );
  }
  return {
    writable: settings.writable,
    providerSettings,
    defaultSettings,
    profiles,
    providers: providerData.providers,
    groups: normalizeCatalogReadiness(modelData.groups, profiles, providerData.providers, credentials),
    failures: modelData.failures,
    credentials,
  };
}

const STYLES = `
.tc-page{box-sizing:border-box;color:var(--dsw-alias-label-primary);font-family:inherit;min-width:0;padding:24px 32px 32px}.tc-page *{box-sizing:border-box}.tc-page-header{align-items:flex-start;display:flex;gap:16px;justify-content:space-between;margin-bottom:24px}.tc-page-title{font-size:22px;line-height:30px;margin:0}.tc-page-intro{color:var(--dsw-alias-label-secondary);font-size:14px;line-height:22px;margin:4px 0 0;max-width:640px}.tc-stack{display:flex;flex-direction:column;gap:16px}.tc-card{border:1px solid var(--dsw-alias-border-l2);border-radius:16px;padding:16px}.tc-card-head{align-items:flex-start;display:flex;gap:16px;justify-content:space-between;margin-bottom:16px}.tc-card-title{font-size:16px;line-height:24px;margin:0;overflow-wrap:anywhere}.tc-subtle{color:var(--dsw-alias-label-tertiary);font-size:13px;line-height:20px;margin:0}.tc-statuses{display:flex;flex-wrap:wrap;gap:8px;margin-top:8px}.tc-status{background:var(--dsw-alias-bg-module-platform);border-radius:12px;color:var(--dsw-alias-label-secondary);font-size:12px;line-height:20px;padding:0 8px}.tc-status[data-tone=good]{color:var(--dsw-alias-state-success-primary)}.tc-status[data-tone=warn]{color:var(--dsw-alias-state-warn-label)}.tc-status[data-tone=bad]{color:var(--dsw-alias-state-error-primary)}.tc-grid{display:grid;gap:12px;grid-template-columns:repeat(2,minmax(0,1fr))}.tc-field{display:flex;flex-direction:column;gap:4px;min-width:0}.tc-field-wide{grid-column:1/-1}.tc-label,.tc-legend{color:var(--dsw-alias-label-secondary);font-size:13px;line-height:20px}.tc-input,.tc-select{background:var(--dsw-specific-input-major);border:1px solid var(--dsw-alias-border-l2);border-radius:12px;color:var(--dsw-alias-label-primary);font:inherit;height:36px;min-width:0;padding:0 12px;width:100%}.tc-input:disabled,.tc-select:disabled,.tc-button:disabled{cursor:not-allowed;opacity:.5}.tc-input:focus-visible,.tc-select:focus-visible,.tc-button:focus-visible,.tc-check input:focus-visible{outline:2px solid var(--dsw-alias-state-business-primary);outline-offset:2px}.tc-section-head{align-items:center;display:flex;gap:12px;justify-content:space-between;margin-top:20px}.tc-section-title{font-size:14px;line-height:22px;margin:0}.tc-model-list{border-top:1px solid var(--dsw-alias-border-l2);margin-top:8px}.tc-model{border:0;border-bottom:1px solid var(--dsw-alias-border-l2);margin:0;padding:16px 0}.tc-model:last-child{border-bottom:0}.tc-model-head{align-items:center;display:flex;gap:12px;justify-content:space-between;margin-bottom:12px}.tc-checks{align-items:center;display:flex;flex-wrap:wrap;gap:16px;min-height:36px}.tc-check{align-items:center;color:var(--dsw-alias-label-secondary);display:flex;font-size:13px;gap:8px}.tc-actions{align-items:center;display:flex;flex-wrap:wrap;gap:8px;margin-top:16px}.tc-actions-end{justify-content:flex-end}.tc-button{background:var(--dsw-alias-bg-module-platform);border:0;border-radius:18px;color:var(--dsw-alias-label-primary);cursor:pointer;font:inherit;font-size:13px;height:36px;padding:0 16px}.tc-button:hover:not(:disabled){background:var(--dsw-alias-interactive-bg-hover)}.tc-button-primary{background:var(--dsw-static-deepseek-500);color:var(--dsw-alias-label-primary-inverted)}.tc-button-primary:hover:not(:disabled){background:var(--dsw-static-blue-450)}.tc-button-danger{color:var(--dsw-alias-state-error-primary)}.tc-button-danger:hover:not(:disabled){background:var(--dsw-alias-interactive-bg-hover-danger)}.tc-button-quiet{background:transparent;height:32px;padding:0 8px}.tc-message{border-radius:12px;font-size:13px;line-height:20px;margin:12px 0 0;padding:8px 12px}.tc-error{background:var(--dsw-alias-interactive-bg-hover-danger);color:var(--dsw-alias-state-error-primary)}.tc-success{background:var(--dsw-alias-bg-module-platform);color:var(--dsw-alias-state-success-primary)}.tc-error-list{margin:0;padding-left:20px}.tc-divider{border-top:1px solid var(--dsw-alias-border-l2);margin-top:16px;padding-top:16px}.tc-candidates{display:flex;flex-direction:column;margin-top:8px}.tc-candidate{align-items:center;border-bottom:1px solid var(--dsw-alias-border-l2);display:flex;gap:12px;justify-content:space-between;padding:12px 0}.tc-candidate:last-child{border-bottom:0}.tc-candidate-copy{min-width:0}.tc-empty{border:1px dashed var(--dsw-alias-border-l2);border-radius:16px;padding:24px;text-align:left}.tc-empty h3{font-size:16px;margin:0 0 4px}.tc-empty p{color:var(--dsw-alias-label-secondary);font-size:14px;line-height:22px;margin:0 0 16px}.tc-inline-confirm{align-items:center;background:var(--dsw-alias-bg-module-platform);border-radius:12px;display:flex;flex-wrap:wrap;gap:8px;margin-top:12px;padding:12px}.tc-inline-confirm p{flex:1;font-size:13px;margin:0;min-width:200px}.tc-default-grid{align-items:end;display:grid;gap:12px;grid-template-columns:minmax(0,1fr) auto}.tc-spinner{color:var(--dsw-alias-label-tertiary);font-size:13px;margin:0}.tc-protocol{align-items:center;background:var(--dsw-alias-bg-module-platform);border-radius:12px;color:var(--dsw-alias-label-secondary);display:flex;font-size:13px;height:36px;padding:0 12px}.tc-credential-note{color:var(--dsw-alias-label-tertiary);font-size:12px;line-height:18px;margin:4px 0 0;overflow-wrap:anywhere}@media(max-width:720px){.tc-page{padding:16px}.tc-page-header,.tc-card-head{align-items:stretch;flex-direction:column}.tc-grid{grid-template-columns:1fr}.tc-default-grid{grid-template-columns:1fr}.tc-field-wide{grid-column:auto}.tc-button{width:100%}.tc-actions .tc-button,.tc-candidate .tc-button,.tc-inline-confirm .tc-button{width:auto}}
`;

function SettingsTrigger({ wide }: { wide: boolean }) {
  return (
    <>
      <svg
        aria-hidden={wide}
        aria-label={wide ? undefined : 'Settings'}
        fill="none"
        height="18"
        role={wide ? undefined : 'img'}
        viewBox="0 0 24 24"
        width="18"
      >
        <path d="M12 8.5a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7Z" stroke="currentColor" strokeWidth="1.6" />
        <path d="M19.1 13.2c.05-.39.05-.81 0-1.2l2-1.55-2-3.46-2.48 1a8 8 0 0 0-2.08-1.2L14.2 4h-4l-.35 2.79A8 8 0 0 0 7.78 8L5.3 7l-2 3.46L5.3 12a8 8 0 0 0 0 2.4l-2 1.55 2 3.46 2.48-1a8 8 0 0 0 2.08 1.2l.35 2.79h4l.35-2.79a8 8 0 0 0 2.08-1.2l2.48 1 2-3.46-2-1.55Z" stroke="currentColor" strokeLinejoin="round" strokeWidth="1.6" />
      </svg>
      {wide && <span>Settings</span>}
    </>
  );
}

function ProviderStatus({
  directory,
  group,
  failure,
  credential,
}: {
  directory?: ProviderDirectoryEntry;
  group?: CatalogGroup;
  failure?: CatalogFailure;
  credential?: CredentialDescriptor;
}) {
  const active = directory?.active ?? false;
  const routable = group?.routable ?? false;
  const configured = credential?.configured ?? group?.credentialConfigured ?? directory?.credentialConfigured ?? false;
  const source = credential?.source === 'env'
    ? 'environment'
    : credential?.source === 'file' ? 'credential store' : undefined;
  const routeFailure = group?.failure ?? failure;
  return (
    <>
      <div className="tc-statuses" aria-label="Provider status">
        <span className="tc-status" data-tone={active ? 'good' : 'warn'}>{active ? 'Active' : 'Inactive'}</span>
        <span className="tc-status" data-tone={routable ? 'good' : 'bad'}>{routable ? 'Routable' : 'Not routable'}</span>
        <span className="tc-status" data-tone={configured ? 'good' : 'warn'}>
          {configured ? `Key configured${source ? ` by ${source}` : ''}` : 'Key not configured'}
        </span>
      </div>
      {routeFailure && <p className="tc-message tc-error" role="alert">{routeFailure.message}</p>}
    </>
  );
}

function ModelFields({
  model,
  index,
  disabled,
  onChange,
  onRemove,
}: {
  model: ProviderModelDraft;
  index: number;
  disabled: boolean;
  onChange: (model: ProviderModelDraft) => void;
  onRemove: () => void;
}) {
  const toggle = (modality: Modality) => {
    const input = model.input.includes(modality)
      ? model.input.filter((item) => item !== modality)
      : [...model.input, modality];
    onChange({ ...model, input });
  };
  return (
    <fieldset className="tc-model">
      <legend className="tc-legend">Model {index + 1}</legend>
      <div className="tc-model-head">
        <span />
        <button className="tc-button tc-button-quiet tc-button-danger" disabled={disabled} onClick={onRemove} type="button">
          Remove
        </button>
      </div>
      <div className="tc-grid">
        <label className="tc-field">
          <span className="tc-label">Model ID</span>
          <input className="tc-input" disabled={disabled} onChange={(event) => onChange({ ...model, id: event.target.value })} required value={model.id} />
        </label>
        <label className="tc-field">
          <span className="tc-label">Display name <span className="tc-subtle">(optional)</span></span>
          <input className="tc-input" disabled={disabled} onChange={(event) => onChange({ ...model, name: event.target.value })} value={model.name} />
        </label>
        <label className="tc-field">
          <span className="tc-label">Context window <span className="tc-subtle">(optional)</span></span>
          <input className="tc-input" disabled={disabled} inputMode="numeric" min="1" onChange={(event) => onChange({ ...model, contextWindow: event.target.value })} step="1" type="number" value={model.contextWindow} />
        </label>
        <label className="tc-field">
          <span className="tc-label">Max output tokens <span className="tc-subtle">(optional)</span></span>
          <input className="tc-input" disabled={disabled} inputMode="numeric" min="1" onChange={(event) => onChange({ ...model, maxTokens: event.target.value })} step="1" type="number" value={model.maxTokens} />
        </label>
        <div className="tc-field tc-field-wide">
          <span className="tc-label">Accepted input</span>
          <div className="tc-checks">
            <label className="tc-check"><input checked={model.input.includes('text')} disabled={disabled} onChange={() => toggle('text')} type="checkbox" />Text</label>
            <label className="tc-check"><input checked={model.input.includes('image')} disabled={disabled} onChange={() => toggle('image')} type="checkbox" />Image</label>
          </div>
        </div>
      </div>
    </fieldset>
  );
}

function ProviderEditor({
  api,
  profile,
  providerId,
  revision,
  writable,
  directory,
  group,
  groups,
  failure,
  credential,
  profiles,
  defaultSettings,
  isNew,
  onRefresh,
  onNewCommitted,
  onFinishNew,
  onCancelNew,
  onNotice,
}: {
  api: IApiClient;
  profile: ProviderProfile;
  providerId: string;
  revision: number;
  writable: boolean;
  directory?: ProviderDirectoryEntry;
  group?: CatalogGroup;
  groups: readonly CatalogGroup[];
  failure?: CatalogFailure;
  credential?: CredentialDescriptor;
  profiles: Readonly<Record<string, ProviderCredentialProfile>>;
  defaultSettings: SettingsView;
  isNew: boolean;
  onRefresh: () => Promise<void>;
  onNewCommitted?: (id: string) => void;
  onFinishNew?: () => void;
  onCancelNew?: () => void;
  onNotice: (message: string) => void;
}) {
  const initialDraft = useMemo(() => profileToDraft(providerId, profile), [profile, providerId]);
  const remoteFingerprint = settingsFingerprint(initialDraft);
  const [draft, setDraft] = useState(initialDraft);
  const draftRef = useRef(draft);
  const [committedFingerprint, setCommittedFingerprint] = useState(() => settingsFingerprint(initialDraft));
  const committedRef = useRef(committedFingerprint);
  const [draftIsNew, setDraftIsNew] = useState(isNew);
  const apiKeyInput = useRef<HTMLInputElement>(null);
  const [apiKeyDirty, setApiKeyDirty] = useState(false);
  const apiKeyDirtyRef = useRef(false);
  const [conflict, setConflict] = useState(false);
  const observedRevisionRef = useRef(revision);
  const observedRemoteFingerprintRef = useRef(remoteFingerprint);
  const pendingSavedFingerprintRef = useRef<string>();
  const discoveryGenerationRef = useRef(0);
  const discoveryCandidateFingerprintRef = useRef('');
  const discoveryCandidateGenerationRef = useRef(0);
  const [errors, setErrors] = useState<string[]>([]);
  const [error, setError] = useState<string>();
  const [success, setSuccess] = useState<string>();
  const [saving, setSaving] = useState(false);
  const [retryCredential, setRetryCredential] = useState(false);
  const [discovering, setDiscovering] = useState(false);
  const [candidates, setCandidates] = useState<DiscoveredModel[]>([]);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const validationId = useId();

  const currentDiscoveryFingerprint = () => discoveryFingerprint(
    draftRef.current,
    apiKeyInput.current?.value ?? '',
  );
  const invalidateDiscovery = () => {
    discoveryGenerationRef.current += 1;
    discoveryCandidateFingerprintRef.current = '';
    discoveryCandidateGenerationRef.current = 0;
    setCandidates([]);
    setDiscovering(false);
  };

  useEffect(() => {
    const observedRevision = observedRevisionRef.current;
    const observedFingerprint = observedRemoteFingerprintRef.current;
    const remoteChanged = revision !== observedRevision || remoteFingerprint !== observedFingerprint;
    if (!remoteChanged) return;
    let ownRefresh = false;
    let pendingMismatch = false;
    if (pendingSavedFingerprintRef.current) {
      if (remoteFingerprint === pendingSavedFingerprintRef.current) {
        pendingSavedFingerprintRef.current = undefined;
        ownRefresh = true;
      } else {
        pendingSavedFingerprintRef.current = undefined;
        pendingMismatch = true;
      }
    }
    const localDirty = settingsFingerprint(draftRef.current) !== committedRef.current || apiKeyDirtyRef.current;
    if (pendingMismatch || (!ownRefresh && hasDraftConflict({
      dirty: localDirty,
      observedRevision,
      remoteRevision: revision,
      observedFingerprint,
      remoteFingerprint,
    }))) {
      observedRevisionRef.current = revision;
      observedRemoteFingerprintRef.current = remoteFingerprint;
      setConflict(true);
      return;
    }
    observedRevisionRef.current = revision;
    observedRemoteFingerprintRef.current = remoteFingerprint;
    const adopted = profileToDraft(providerId, profile);
    draftRef.current = adopted;
    committedRef.current = remoteFingerprint;
    setDraft(adopted);
    setCommittedFingerprint(remoteFingerprint);
    setConflict(false);
    invalidateDiscovery();
  }, [initialDraft, profile, providerId, remoteFingerprint, revision]);

  const reloadRemote = () => {
    const adopted = profileToDraft(providerId, profile);
    draftRef.current = adopted;
    committedRef.current = remoteFingerprint;
    observedRevisionRef.current = revision;
    observedRemoteFingerprintRef.current = remoteFingerprint;
    pendingSavedFingerprintRef.current = undefined;
    apiKeyDirtyRef.current = false;
    setApiKeyDirty(false);
    if (apiKeyInput.current) apiKeyInput.current.value = '';
    setDraft(adopted);
    setCommittedFingerprint(remoteFingerprint);
    setConflict(false);
    setErrors([]);
    setError(undefined);
    setSuccess(undefined);
    invalidateDiscovery();
  };

  const changeDraft = (update: (current: ProviderDraft) => ProviderDraft) => {
    const current = draftRef.current;
    const next = update(current);
    if (discoveryFingerprint(current, apiKeyInput.current?.value ?? '') !== discoveryFingerprint(next, apiKeyInput.current?.value ?? '')) {
      invalidateDiscovery();
    }
    draftRef.current = next;
    setDraft(next);
    setErrors([]);
    setError(undefined);
    setSuccess(undefined);
  };

  const updateModel = (index: number, model: ProviderModelDraft) => {
    changeDraft((current) => ({
      ...current,
      models: current.models.map((item, itemIndex) => itemIndex === index ? model : item),
    }));
  };

  const adoptCandidate = (candidate: DiscoveredModel) => {
    if (!isCurrentDiscoveryResult(
      discoveryCandidateFingerprintRef.current,
      currentDiscoveryFingerprint(),
      discoveryCandidateGenerationRef.current,
      discoveryGenerationRef.current,
    )) return;
    if (draft.models.some((model) => model.id === candidate.id)) return;
    const adopted: ProviderModelDraft = {
      id: candidate.id,
      name: candidate.name ?? '',
      input: ['text'],
      contextWindow: candidate.contextWindow === undefined ? '' : String(candidate.contextWindow),
      maxTokens: candidate.maxTokens === undefined ? '' : String(candidate.maxTokens),
    };
    changeDraft((current) => ({
      ...current,
      models: current.models.length === 1 && !current.models[0].id.trim()
        ? [adopted]
        : [...current.models, adopted],
    }));
  };

  const discover = async () => {
    const urlError = baseUrlError(draft.baseURL);
    if (urlError) {
      setError(urlError);
      return;
    }
    const typedKey = apiKeyInput.current?.value ?? '';
    const keyError = validateApiKey(typedKey);
    if (keyError) {
      setError(keyError);
      setSuccess(undefined);
      return;
    }
    const fingerprint = discoveryFingerprint(draft, typedKey);
    const generation = ++discoveryGenerationRef.current;
    discoveryCandidateFingerprintRef.current = '';
    discoveryCandidateGenerationRef.current = 0;
    setCandidates([]);
    setDiscovering(true);
    setError(undefined);
    setSuccess(undefined);
    try {
      const response = await api.llm.discoverModels({
        settingsNs: PROVIDER_SETTINGS_NS,
        provider: draftIsNew ? undefined : draft.id.trim(),
        baseURL: draft.baseURL.trim(),
        api: PROVIDER_API,
        apiKey: typedKey.trim() ? typedKey : undefined,
      });
      const value = unwrap(response as RpcResponse<{ models: DiscoveredModel[] }>);
      if (!isCurrentDiscoveryResult(
        fingerprint,
        currentDiscoveryFingerprint(),
        generation,
        discoveryGenerationRef.current,
      )) return;
      discoveryCandidateFingerprintRef.current = fingerprint;
      discoveryCandidateGenerationRef.current = generation;
      setCandidates(value.models);
      setSuccess(value.models.length
        ? `Found ${value.models.length} model${value.models.length === 1 ? '' : 's'}. Choose which to adopt.`
        : 'The provider returned no models. You can still add one manually.');
    } catch (caught) {
      if (isCurrentDiscoveryResult(
        fingerprint,
        currentDiscoveryFingerprint(),
        generation,
        discoveryGenerationRef.current,
      )) setError(publicError(caught, [typedKey]));
    } finally {
      if (generation === discoveryGenerationRef.current) setDiscovering(false);
    }
  };

  const apply = async () => {
    const localDirty = settingsFingerprint(draft) !== committedRef.current || apiKeyDirty;
    const remoteConflict = hasDraftConflict({
      dirty: localDirty,
      observedRevision: observedRevisionRef.current,
      remoteRevision: revision,
      observedFingerprint: observedRemoteFingerprintRef.current,
      remoteFingerprint,
    });
    if (conflict || remoteConflict) {
      setConflict(true);
      setErrors([]);
      setError('Remote provider changes were detected. Reload the draft before applying it.');
      setSuccess(undefined);
      return;
    }
    const validation = [
      ...validateProviderDraft(draft),
      ...validateProviderIdentity(draft, profiles, {
        isNew: draftIsNew,
        originalId: draftIsNew ? undefined : providerId,
        persistedCredentialRef: draftIsNew ? undefined : profile.apiKeyEnv,
      }),
    ];
    if (validation.length) {
      setErrors([...new Set(validation)]);
      setError(undefined);
      setSuccess(undefined);
      return;
    }
    const id = draft.id.trim();
    const profileValue = profileFromDraft(draft);
    const settingsDirty = settingsFingerprint(draft) !== committedRef.current;
    const typedKey = apiKeyInput.current?.value ?? '';
    const keyError = validateApiKey(typedKey);
    if (keyError) {
      setError(keyError);
      setSuccess(undefined);
      return;
    }
    const keyDirty = Boolean(typedKey.trim());
    if (!settingsDirty && !keyDirty) {
      setSuccess('No changes to apply.');
      return;
    }

    setSaving(true);
    setError(undefined);
    setSuccess(undefined);
    let settingsWritten = false;
    let credentialAttempted = false;
    const wasNew = draftIsNew;
    try {
      if (settingsDirty) {
        unwrap(await api.settings.mutate({
          ns: PROVIDER_SETTINGS_NS,
          ops: [{ op: 'set', path: ['providers', id], value: profileValue }],
          expectedRevision: revision,
        }) as RpcResponse<SettingsView>);
        settingsWritten = true;
        const savedDraft = profileToDraft(id, profileValue);
        const savedFingerprint = settingsFingerprint(savedDraft);
        pendingSavedFingerprintRef.current = savedFingerprint;
        draftRef.current = savedDraft;
        committedRef.current = savedFingerprint;
        setDraft(savedDraft);
        setCommittedFingerprint(savedFingerprint);
        setConflict(false);
        if (wasNew) {
          setDraftIsNew(false);
          onNewCommitted?.(id);
        }
      }
      if (keyDirty) {
        credentialAttempted = true;
        unwrap(await api.credentials.set({ ref: profileValue.apiKeyEnv, value: typedKey }) as RpcResponse<{}>);
        apiKeyDirtyRef.current = false;
        setApiKeyDirty(false);
        setRetryCredential(false);
        if (apiKeyInput.current) apiKeyInput.current.value = '';
      }
      const message = settingsWritten && keyDirty
        ? 'Provider and API key saved.'
        : settingsWritten ? 'Provider saved.' : 'API key saved.';
      setSuccess(message);
      if (onFinishNew) {
        onFinishNew();
        onNotice(message);
      }
    } catch (caught) {
      const detail = publicError(caught, [typedKey]);
      if (settingsWritten && credentialAttempted) setRetryCredential(true);
      setError(settingsWritten && credentialAttempted
        ? `Provider settings were saved, but the API key was not saved. ${detail}`
        : detail);
    } finally {
      if (settingsWritten || credentialAttempted) await onRefresh();
      setSaving(false);
    }
  };

  const remove = async () => {
    const localDirty = settingsFingerprint(draft) !== committedRef.current || apiKeyDirty;
    const remoteConflict = hasDraftConflict({
      dirty: localDirty,
      observedRevision: observedRevisionRef.current,
      remoteRevision: revision,
      observedFingerprint: observedRemoteFingerprintRef.current,
      remoteFingerprint,
    });
    if (conflict || remoteConflict) {
      setConflict(true);
      setError('Remote provider changes were detected. Reload the draft before deleting it.');
      return;
    }
    const defaultDecision = decideDefaultAfterProviderDelete(
      draft.id.trim(),
      readDefaultSelection(defaultSettings.value),
      groups,
    );
    if (defaultDecision.kind === 'block') {
      setError(defaultDecision.notice);
      setSuccess(undefined);
      setConfirmDelete(false);
      return;
    }
    setSaving(true);
    setError(undefined);
    setSuccess(undefined);
    const ref = draft.apiKeyEnv;
    let defaultReplaced = false;
    let providerRemoved = false;
    let credentialAttempted = false;
    try {
      // Replace the default first so a successful route removal can never leave an orphaned selection.
      if (defaultDecision.kind === 'replace') {
        unwrap(await api.settings.mutate({
          ns: DEFAULT_MODEL_SETTINGS_NS,
          ops: [{ op: 'set', path: [], value: defaultDecision.selection }],
          expectedRevision: defaultSettings.revision,
        }) as RpcResponse<SettingsView>);
        defaultReplaced = true;
      }
      unwrap(await api.settings.mutate({
        ns: PROVIDER_SETTINGS_NS,
        ops: [{ op: 'unset', path: ['providers', draft.id] }],
        expectedRevision: revision,
      }) as RpcResponse<SettingsView>);
      providerRemoved = true;
      if (ref === deriveCredentialRef(draft.id.trim()) && credential?.writable) {
        credentialAttempted = true;
        unwrap(await api.credentials.unset({ ref }) as RpcResponse<{}>);
      }
      onNotice(`Provider “${draft.displayName}” deleted.${defaultDecision.kind === 'replace' ? ` ${defaultDecision.notice}` : ''}`);
    } catch (caught) {
      const detail = publicError(caught);
      if (providerRemoved && credentialAttempted) {
        const warning = `The provider was deleted, but its writable credential could not be removed. ${detail}`;
        setError(warning);
        onNotice(warning);
      } else if (defaultDecision.kind === 'replace' && defaultReplaced && !providerRemoved) {
        const warning = `${defaultDecision.notice} The provider was not deleted. ${detail}`;
        setError(warning);
        onNotice(warning);
      } else {
        setError(detail);
      }
    } finally {
      await onRefresh();
      setSaving(false);
      setConfirmDelete(false);
    }
  };

  const draftDirty = settingsFingerprint(draft) !== committedRef.current || apiKeyDirty;
  const remoteConflictNow = !pendingSavedFingerprintRef.current && hasDraftConflict({
    dirty: draftDirty,
    observedRevision: observedRevisionRef.current,
    remoteRevision: revision,
    observedFingerprint: observedRemoteFingerprintRef.current,
    remoteFingerprint,
  });
  const disabled = !writable || saving || conflict || remoteConflictNow;
  const effectiveCredential = draftIsNew ? deriveCredentialRef(draft.id.trim()) : draft.apiKeyEnv;
  const title = draft.displayName.trim() || (draftIsNew ? 'New custom provider' : draft.id);
  return (
    <article className="tc-card" aria-labelledby={`${validationId}-title`}>
      <div className="tc-card-head">
        <div>
          <h3 className="tc-card-title" id={`${validationId}-title`}>{title}</h3>
          <p className="tc-subtle">{draftIsNew ? 'Not saved yet' : draft.id}</p>
          {!draftIsNew && <ProviderStatus credential={credential} directory={directory} failure={failure} group={group} />}
        </div>
        {onCancelNew && (
          <button className="tc-button tc-button-quiet" disabled={saving} onClick={onCancelNew} type="button">
            {draftIsNew ? 'Cancel' : 'Close'}
          </button>
        )}
      </div>

      {(conflict || remoteConflictNow) && (
        <div className="tc-message tc-error" role="alert">
          <p>Remote provider settings changed while this draft was being edited. Reload before applying or deleting it.</p>
          <button className="tc-button" disabled={saving} onClick={onCancelNew && draftIsNew ? onCancelNew : reloadRemote} type="button">
            {onCancelNew && draftIsNew ? 'Cancel draft' : 'Reload remote'}
          </button>
        </div>
      )}

      <div className="tc-grid">
        <label className="tc-field">
          <span className="tc-label">Provider ID</span>
          <input
            aria-describedby={errors.length ? validationId : undefined}
            aria-invalid={errors.length > 0}
            className="tc-input"
            disabled={disabled || !draftIsNew}
            onChange={(event) => changeDraft((current) => ({
              ...current,
              id: event.target.value,
              apiKeyEnv: deriveCredentialRef(event.target.value.trim()),
            }))}
            required
            spellCheck={false}
            value={draft.id}
          />
        </label>
        <label className="tc-field">
          <span className="tc-label">Display name</span>
          <input className="tc-input" disabled={disabled} onChange={(event) => changeDraft((current) => ({ ...current, displayName: event.target.value }))} required value={draft.displayName} />
        </label>
        <label className="tc-field tc-field-wide">
          <span className="tc-label">Base URL</span>
          <input className="tc-input" disabled={disabled} inputMode="url" onChange={(event) => changeDraft((current) => ({ ...current, baseURL: event.target.value }))} placeholder="https://provider.example/v1" required spellCheck={false} type="url" value={draft.baseURL} />
        </label>
        <div className="tc-field">
          <span className="tc-label">API</span>
          <div className="tc-protocol">OpenAI Responses</div>
        </div>
        <label className="tc-field">
          <span className="tc-label">API key <span className="tc-subtle">(write-only)</span></span>
          <input
            autoComplete="off"
            className="tc-input"
            disabled={disabled || credential?.writable === false}
            onChange={(event) => {
              const dirty = event.currentTarget.value.length > 0;
              apiKeyDirtyRef.current = dirty;
              setApiKeyDirty(dirty);
              invalidateDiscovery();
              setError(undefined);
              setSuccess(undefined);
            }}
            placeholder={credential?.configured ? 'Leave blank to preserve' : 'Leave blank to configure later'}
            ref={apiKeyInput}
            spellCheck={false}
            type="password"
          />
          <span className="tc-credential-note">
            Credential reference: {effectiveCredential || 'derived after entering an ID'}
            {credential?.writable === false ? ' · Managed by a read-only source' : ''}
          </span>
        </label>
      </div>

      <div className="tc-section-head">
        <h4 className="tc-section-title">Models</h4>
        <button
          className="tc-button tc-button-quiet"
          disabled={disabled}
          onClick={() => changeDraft((current) => ({
            ...current,
            models: [...current.models, { id: '', name: '', input: ['text'], contextWindow: '', maxTokens: '' }],
          }))}
          type="button"
        >
          Add model
        </button>
      </div>
      <div className="tc-model-list">
        {draft.models.map((model, index) => (
          <ModelFields
            disabled={disabled}
            index={index}
            key={index}
            model={model}
            onChange={(next) => updateModel(index, next)}
            onRemove={() => changeDraft((current) => ({ ...current, models: current.models.filter((_, itemIndex) => itemIndex !== index) }))}
          />
        ))}
      </div>

      <div className="tc-divider">
        <div className="tc-section-head">
          <div>
            <h4 className="tc-section-title">Available models</h4>
            <p className="tc-subtle">Discovery uses this unsaved URL and typed key. Nothing is adopted automatically.</p>
          </div>
          <button className="tc-button" disabled={disabled || discovering} onClick={() => void discover()} type="button">
            {discovering ? 'Discovering…' : 'Discover models'}
          </button>
        </div>
        {candidates.length > 0 && (
          <div className="tc-candidates">
            {candidates.map((candidate) => {
              const adopted = draft.models.some((model) => model.id === candidate.id);
              const capacity = [
                candidate.contextWindow ? `${candidate.contextWindow.toLocaleString()} context` : '',
                candidate.maxTokens ? `${candidate.maxTokens.toLocaleString()} output` : '',
              ].filter(Boolean).join(' · ');
              return (
                <div className="tc-candidate" key={candidate.id}>
                  <div className="tc-candidate-copy">
                    <strong>{candidate.name || candidate.id}</strong>
                    {candidate.name && <p className="tc-subtle">{candidate.id}</p>}
                    {capacity && <p className="tc-subtle">{capacity}</p>}
                  </div>
                  <button className="tc-button" disabled={disabled || adopted} onClick={() => adoptCandidate(candidate)} type="button">
                    {adopted ? 'Adopted' : 'Adopt'}
                  </button>
                </div>
              );
            })}
          </div>
        )}
      </div>

      {errors.length > 0 && (
        <div className="tc-message tc-error" id={validationId} role="alert">
          <ul className="tc-error-list">{errors.map((item) => <li key={item}>{item}</li>)}</ul>
        </div>
      )}
      {error && <p className="tc-message tc-error" role="alert">{error}</p>}
      {success && <p className="tc-message tc-success" role="status">{success}</p>}

      <div className="tc-actions tc-actions-end">
        {!draftIsNew && !onCancelNew && (
          <button className="tc-button tc-button-danger" disabled={disabled} onClick={() => setConfirmDelete(true)} type="button">Delete provider</button>
        )}
        <button className="tc-button tc-button-primary" disabled={disabled} onClick={() => void apply()} type="button">
          {saving ? 'Applying…' : retryCredential ? 'Retry API key' : 'Apply'}
        </button>
      </div>
      {confirmDelete && (
        <div className="tc-inline-confirm" role="group" aria-label="Confirm provider deletion">
          <p>Delete this provider? Existing sessions that selected it will stop routing.</p>
          <button className="tc-button" disabled={saving} onClick={() => setConfirmDelete(false)} type="button">Cancel</button>
          <button className="tc-button tc-button-danger" disabled={saving} onClick={() => void remove()} type="button">Delete</button>
        </div>
      )}
    </article>
  );
}

function DefaultModelEditor({
  api,
  settings,
  groups,
  writable,
  onRefresh,
}: {
  api: IApiClient;
  settings: SettingsView;
  groups: CatalogGroup[];
  writable: boolean;
  onRefresh: () => Promise<void>;
}) {
  const remote = readDefaultSelection(settings.value);
  const remoteKey = selectionKey(remote);
  const [selection, setSelection] = useState<DefaultSelection | undefined>(remote);
  const [baseline, setBaseline] = useState(remoteKey);
  const [conflict, setConflict] = useState(false);
  const observedRevisionRef = useRef(settings.revision);
  const observedRemoteKeyRef = useRef(remoteKey);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string>();
  const [success, setSuccess] = useState<string>();

  useEffect(() => {
    const observedRevision = observedRevisionRef.current;
    const observedKey = observedRemoteKeyRef.current;
    const remoteChanged = settings.revision !== observedRevision || remoteKey !== observedKey;
    if (!remoteChanged) return;
    const dirty = selectionKey(selection) !== baseline;
    if (hasDraftConflict({
      dirty,
      observedRevision,
      remoteRevision: settings.revision,
      observedFingerprint: observedKey,
      remoteFingerprint: remoteKey,
    })) {
      observedRevisionRef.current = settings.revision;
      observedRemoteKeyRef.current = remoteKey;
      setConflict(true);
      return;
    }
    observedRevisionRef.current = settings.revision;
    observedRemoteKeyRef.current = remoteKey;
    setSelection(remote);
    setBaseline(remoteKey);
    setConflict(false);
  }, [remoteKey, settings.revision]);

  const reloadRemote = () => {
    observedRevisionRef.current = settings.revision;
    observedRemoteKeyRef.current = remoteKey;
    setSelection(remote);
    setBaseline(remoteKey);
    setConflict(false);
    setError(undefined);
    setSuccess(undefined);
  };

  const routableGroups = groups.flatMap((group) => {
    const models = group.routable === true
      ? group.models.filter((model) => model.routable === true)
      : [];
    return models.length ? [{ ...group, models }] : [];
  });
  const options = routableGroups.flatMap((group) => group.models.map((model) => ({
    group,
    model,
    value: selectionKey({ provider: group.id, model: model.id }),
  })));
  const currentValue = selectionKey(selection);
  const currentAvailable = options.some((option) => option.value === currentValue);
  const remoteConflictNow = hasDraftConflict({
    dirty: currentValue !== baseline,
    observedRevision: observedRevisionRef.current,
    remoteRevision: settings.revision,
    observedFingerprint: observedRemoteKeyRef.current,
    remoteFingerprint: remoteKey,
  });

  const save = async () => {
    if (conflict || remoteConflictNow) {
      setConflict(true);
      setError('Remote default model changes were detected. Reload the draft before saving it.');
      setSuccess(undefined);
      return;
    }
    if (!selection) {
      setError('Choose a default model.');
      return;
    }
    if (currentValue === baseline) {
      setSuccess('No changes to apply.');
      return;
    }
    if (!currentAvailable) {
      setError('Choose a routable default model.');
      return;
    }
    setSaving(true);
    setError(undefined);
    setSuccess(undefined);
    try {
      unwrap(await api.settings.mutate({
        ns: DEFAULT_MODEL_SETTINGS_NS,
        ops: [{ op: 'set', path: [], value: selection }],
        expectedRevision: settings.revision,
      }) as RpcResponse<SettingsView>);
      setBaseline(currentValue);
      setConflict(false);
      setSuccess('Default model saved for new sessions.');
    } catch (caught) {
      setError(publicError(caught));
    } finally {
      await onRefresh();
      setSaving(false);
    }
  };

  return (
    <section className="tc-card" aria-labelledby="tc-default-title">
      <div className="tc-card-head">
        <div>
          <h3 className="tc-card-title" id="tc-default-title">Default model</h3>
          <p className="tc-subtle">Used when a new session is created. Existing sessions keep their selection.</p>
        </div>
      </div>
      {conflict || remoteConflictNow ? (
        <div className="tc-message tc-error" role="alert">
          <p>Remote default model settings changed while this draft was being edited. Reload before saving it.</p>
          <button className="tc-button" disabled={saving} onClick={reloadRemote} type="button">Reload remote</button>
        </div>
      ) : null}
      <div className="tc-default-grid">
        <label className="tc-field">
          <span className="tc-label">Provider and model</span>
          <select
            className="tc-select"
            disabled={!writable || saving || conflict || remoteConflictNow || options.length === 0}
            onChange={(event) => {
              setSelection(JSON.parse(event.target.value) as DefaultSelection);
              setError(undefined);
              setSuccess(undefined);
            }}
            value={currentValue}
          >
            {!selection && <option value="">Choose a model</option>}
            {selection && !currentAvailable && <option disabled value={currentValue}>Unavailable: {selection.provider} / {selection.model}</option>}
            {routableGroups.map((group) => (
              <optgroup key={group.id} label={group.name}>
                {group.models.map((model) => (
                  <option key={model.id} value={selectionKey({ provider: group.id, model: model.id })}>{model.name || model.id}</option>
                ))}
              </optgroup>
            ))}
          </select>
        </label>
        <button className="tc-button tc-button-primary" disabled={!writable || saving || conflict || remoteConflictNow || options.length === 0} onClick={() => void save()} type="button">
          {saving ? 'Saving…' : 'Save default'}
        </button>
      </div>
      {options.length === 0 && <p className="tc-message tc-error" role="status">Configure a routable provider and model before choosing a default.</p>}
      {error && <p className="tc-message tc-error" role="alert">{error}</p>}
      {success && <p className="tc-message tc-success" role="status">{success}</p>}
    </section>
  );
}

function ProviderSettingsPage({ ctx, api }: { ctx: Context; api: IApiClient }) {
  const [snapshot, setSnapshot] = useState<ProviderSettingsSnapshot>();
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string>();
  const [showNew, setShowNew] = useState(false);
  const [pendingNewId, setPendingNewId] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const request = useRef(0);

  const refresh = useCallback(async () => {
    const generation = ++request.current;
    setLoading(true);
    try {
      const next = await loadSnapshot(api);
      if (generation !== request.current) return;
      setSnapshot(next);
      setLoadError(undefined);
    } catch (caught) {
      if (generation === request.current) setLoadError(publicError(caught));
    } finally {
      if (generation === request.current) setLoading(false);
    }
  }, [api]);

  useEffect(() => {
    void refresh();
    const disposers = [
      ctx.on('settings/changed', (namespace) => {
        if (namespace === PROVIDER_SETTINGS_NS || namespace === DEFAULT_MODEL_SETTINGS_NS) void refresh();
      }),
      ctx.on('credentials/changed', () => void refresh()),
      ctx.on('models/changed', () => void refresh()),
      ctx.on('connection/reset', () => void refresh()),
    ];
    return () => disposers.forEach((dispose) => dispose());
  }, [ctx, refresh]);

  if (!snapshot) {
    return (
      <div className="tc-page">
        <style>{STYLES}</style>
        {loading && <p className="tc-spinner" role="status">Loading model settings…</p>}
        {loadError && (
          <div className="tc-message tc-error" role="alert">
            <p>{loadError}</p>
            <button className="tc-button" onClick={() => void refresh()} type="button">Retry</button>
          </div>
        )}
      </div>
    );
  }

  const profiles = Object.entries(snapshot.profiles)
    .filter(([id]) => id !== pendingNewId)
    .sort((left, right) => left[1].displayName.localeCompare(right[1].displayName));
  const pendingProfile = pendingNewId ? snapshot.profiles[pendingNewId] : undefined;
  const newDirectory = pendingNewId
    ? snapshot.providers.find((provider) => provider.provider === pendingNewId)
    : undefined;
  const newGroup = pendingNewId ? snapshot.groups.find((group) => group.id === pendingNewId) : undefined;
  const newFailure = pendingNewId ? snapshot.failures.find((failure) => failure.id === pendingNewId) : undefined;
  const newCredential = pendingProfile ? snapshot.credentials[pendingProfile.apiKeyEnv] : undefined;

  return (
    <div className="tc-page">
      <style>{STYLES}</style>
      <header className="tc-page-header">
        <div>
          <h2 className="tc-page-title">Models</h2>
          <p className="tc-page-intro">Configure OpenAI Responses-compatible providers, their model capabilities, and the default for new sessions.</p>
        </div>
        <button className="tc-button tc-button-primary" disabled={!snapshot.writable || showNew} onClick={() => {
          setShowNew(true);
          setPendingNewId(undefined);
          setNotice(undefined);
        }} type="button">Add provider</button>
      </header>

      {loading && <p className="tc-spinner" role="status">Refreshing…</p>}
      {loadError && <p className="tc-message tc-error" role="alert">Could not refresh: {loadError}</p>}
      {!snapshot.writable && <p className="tc-message tc-error" role="status">This host exposes settings as read-only.</p>}
      {notice && <p className="tc-message tc-success" role="status">{notice}</p>}

      <div className="tc-stack">
        <DefaultModelEditor api={api} groups={snapshot.groups} onRefresh={refresh} settings={snapshot.defaultSettings} writable={snapshot.writable} />

        {profiles.length === 0 && !showNew && (
          <div className="tc-empty">
            <h3>No providers configured</h3>
            <p>Add an OpenAI Responses-compatible endpoint, then define or discover the models it serves.</p>
            <button className="tc-button tc-button-primary" disabled={!snapshot.writable} onClick={() => setShowNew(true)} type="button">Add your first provider</button>
          </div>
        )}

        {showNew && (
          <ProviderEditor
            api={api}
            credential={newCredential}
            defaultSettings={snapshot.defaultSettings}
            directory={newDirectory}
            failure={newFailure}
            group={newGroup}
            groups={snapshot.groups}
            isNew={!pendingNewId}
            profiles={snapshot.profiles}
            onCancelNew={() => {
              setShowNew(false);
              setPendingNewId(undefined);
            }}
            onFinishNew={() => {
              setShowNew(false);
              setPendingNewId(undefined);
            }}
            onNewCommitted={setPendingNewId}
            onNotice={setNotice}
            onRefresh={refresh}
            profile={pendingProfile ?? NEW_PROVIDER_PROFILE}
            providerId={pendingNewId ?? ''}
            revision={snapshot.providerSettings.revision}
            writable={snapshot.writable}
          />
        )}

        {profiles.map(([id, profile]) => (
          <ProviderEditor
            api={api}
            credential={snapshot.credentials[profile.apiKeyEnv]}
            defaultSettings={snapshot.defaultSettings}
            directory={snapshot.providers.find((provider) => provider.provider === id)}
            failure={snapshot.failures.find((failure) => failure.id === id)}
            group={snapshot.groups.find((group) => group.id === id)}
            groups={snapshot.groups}
            isNew={false}
            key={id}
            onNotice={setNotice}
            onRefresh={refresh}
            profile={profile}
            profiles={snapshot.profiles}
            providerId={id}
            revision={snapshot.providerSettings.revision}
            writable={snapshot.writable}
          />
        ))}
      </div>
    </div>
  );
}

export function registerProviderSettings(ctx: Context, api: IApiClient): void {
  ctx.slots.inject('settings.trigger', () => ctx.slots.register(
    { name: 'settings.trigger' },
    ({ wide }) => <SettingsTrigger wide={wide} />,
  ));
  ctx.slots.inject('settings.header', () => ctx.slots.register(
    { name: 'settings.header' },
    () => <>Settings</>,
  ));
  ctx.slots.inject('settings.close', () => ctx.slots.register(
    { name: 'settings.close' },
    () => <>Close</>,
  ));
  ctx.slots.inject('settings.section', () => ctx.slots.register(
    { name: 'settings.section', id: 'models', order: 20, label: 'Models' },
    () => <ProviderSettingsPage api={api} ctx={ctx} />,
  ));
}
