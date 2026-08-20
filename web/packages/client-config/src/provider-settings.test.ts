import { describe, expect, test } from 'bun:test';
import {
  decideDefaultAfterProviderDelete,
  deriveCredentialRef,
  discoveryFingerprint,
  hasDraftConflict,
  isCurrentDiscoveryResult,
  normalizeCatalogReadiness,
  validateApiKey,
  validateProviderDraft,
  validateProviderIdentity,
  type ProviderDraft,
} from './provider-settings';

const validDraft = (): ProviderDraft => ({
  id: 'custom-relay',
  displayName: 'Custom relay',
  baseURL: 'https://relay.example/v1',
  apiKeyEnv: deriveCredentialRef('custom-relay'),
  models: [{
    id: 'vision-model',
    name: 'Vision model',
    input: ['text', 'image'],
    contextWindow: '131072',
    maxTokens: '16384',
  }],
});

const catalogGroup = {
  id: 'custom-relay',
  name: 'Custom relay',
  models: [
    { id: 'text-model', name: 'Text model' },
    { id: 'vision-model', name: 'Vision model' },
  ],
  failure: {
    id: 'custom-relay',
    name: 'Custom relay',
    message: 'provider credential is not configured',
  },
};

const providerProfile = {
  displayName: 'Custom relay',
  baseURL: 'https://relay.example/v1',
  apiKeyEnv: 'CUSTOM_RELAY_API_KEY',
  models: [],
};

const providerDirectory = {
  provider: 'custom-relay',
  displayName: 'Custom relay',
  settingsNs: 'llm-openai-responses',
  settingsPath: ['providers', 'custom-relay'],
  active: true,
};

const credentialDescriptors = (configured: boolean) => ({
  CUSTOM_RELAY_API_KEY: { configured, writable: true },
});

describe('normalizeCatalogReadiness', () => {
  test('marks configured active providers and all of their models routable', () => {
    expect(normalizeCatalogReadiness(
      [catalogGroup],
      { 'custom-relay': providerProfile },
      [providerDirectory],
      credentialDescriptors(true),
    )).toEqual([{
      ...catalogGroup,
      credentialConfigured: true,
      routable: true,
      models: catalogGroup.models.map((model) => ({ ...model, routable: true })),
    }]);
  });

  test('preserves diagnostic groups while disabling unconfigured providers and models', () => {
    expect(normalizeCatalogReadiness(
      [catalogGroup],
      { 'custom-relay': providerProfile },
      [providerDirectory],
      credentialDescriptors(false),
    )).toEqual([{
      ...catalogGroup,
      credentialConfigured: false,
      routable: false,
      models: catalogGroup.models.map((model) => ({ ...model, routable: false })),
    }]);
  });

  test('does not infer readiness when the catalog group has no saved profile', () => {
    const [group] = normalizeCatalogReadiness(
      [catalogGroup],
      {},
      [providerDirectory],
      credentialDescriptors(true),
    );

    expect(group?.routable).toBe(false);
    expect(group?.models.map((model) => model.routable)).toEqual([false, false]);
  });

  test('keeps configured providers unavailable when their directory route is inactive', () => {
    const [group] = normalizeCatalogReadiness(
      [catalogGroup],
      { 'custom-relay': providerProfile },
      [{ ...providerDirectory, active: false }],
      credentialDescriptors(true),
    );

    expect(group?.credentialConfigured).toBe(true);
    expect(group?.routable).toBe(false);
    expect(group?.models.map((model) => model.routable)).toEqual([false, false]);
  });
});

describe('deriveCredentialRef', () => {
  test('uppercases the provider id and replaces every non-alphanumeric character', () => {
    expect(deriveCredentialRef('openai-responses')).toBe('OPENAI_RESPONSES_API_KEY');
    expect(deriveCredentialRef('relay.eu/v2')).toBe('RELAY_EU_V2_API_KEY');
    expect(deriveCredentialRef('a--b')).toBe('A__B_API_KEY');
  });
});

describe('validateApiKey', () => {
  test('accepts blank values and printable ASCII keys', () => {
    expect(validateApiKey('')).toBeUndefined();
    expect(validateApiKey(' \t ')).toBeUndefined();
    expect(validateApiKey('sk-live_123-./+=!~')).toBeUndefined();
  });

  test('rejects leading and trailing whitespace without echoing the key', () => {
    for (const key of [' sk-secret', 'sk-secret\t']) {
      const error = validateApiKey(key);
      expect(error).toBe('API key cannot contain leading or trailing whitespace.');
      expect(error).not.toContain(key);
    }
  });

  test('rejects bytes outside printable ASCII without echoing the key', () => {
    for (const key of ['sk secret', 'sk-\x7f', 'sk-é']) {
      const error = validateApiKey(key);
      expect(error).toBe('API key must contain printable ASCII characters only.');
      expect(error).not.toContain(key);
    }
  });

  test('rejects whole environment assignments without echoing the key', () => {
    for (const key of ['OPENAI_API_KEY=sk-secret', 'api_key=sk-secret']) {
      const error = validateApiKey(key);
      expect(error).toBe('Paste only the API key, without a NAME= prefix.');
      expect(error).not.toContain(key);
    }
  });

  test('rejects paired surrounding quotes without echoing the key', () => {
    for (const key of ['"sk-secret"', "'sk-secret'"]) {
      const error = validateApiKey(key);
      expect(error).toBe('Paste the API key without surrounding quotes.');
      expect(error).not.toContain(key);
    }
  });
});

describe('validateProviderDraft', () => {
  test('accepts a complete HTTP(S) provider and optional safe capacities', () => {
    expect(validateProviderDraft(validDraft())).toEqual([]);
  });

  test('rejects unsafe URLs, invalid derived references, and incomplete models', () => {
    const draft = validDraft();
    draft.id = '123-relay';
    draft.apiKeyEnv = deriveCredentialRef(draft.id);
    draft.baseURL = 'https://user:secret@relay.example/v1?key=value#fragment';
    draft.models = [
      { id: 'duplicate', name: '', input: [], contextWindow: '0', maxTokens: '' },
      { id: 'duplicate', name: '', input: ['text'], contextWindow: '', maxTokens: '9007199254740992' },
    ];

    expect(validateProviderDraft(draft)).toEqual(expect.arrayContaining([
      'Provider ID must derive a valid credential reference (begin with a letter or underscore).',
      'Base URL must be HTTP(S) and cannot include credentials, a query, or a fragment.',
      'Model 1 must accept text or image input.',
      'Model 1 context window must be a positive safe integer.',
      'Model ID “duplicate” is duplicated.',
      'Model 2 max output tokens must be a positive safe integer.',
    ]));
  });

  test('requires provider identity, display name, URL, and at least one model', () => {
    expect(validateProviderDraft({
      id: '',
      displayName: ' ',
      baseURL: '',
      apiKeyEnv: deriveCredentialRef(''),
      models: [],
    })).toEqual([
      'Provider ID is required.',
      'Display name is required.',
      'Base URL is required.',
      'Add at least one model.',
    ]);
  });
});


describe('provider identity guards', () => {
  test('rejects duplicate saved IDs before a new route can overwrite one', () => {
    const errors = validateProviderIdentity(
      { id: 'custom-relay', apiKeyEnv: deriveCredentialRef('custom-relay') },
      { 'custom-relay': { apiKeyEnv: 'CUSTOM_RELAY_API_KEY' } },
      { isNew: true },
    );

    expect(errors).toContain('A provider with this ID already exists.');
  });

  test('rejects a new ID whose derived credential reference belongs to another route', () => {
    const errors = validateProviderIdentity(
      { id: 'foo-bar', apiKeyEnv: deriveCredentialRef('foo-bar') },
      { legacy: { apiKeyEnv: 'FOO_BAR_API_KEY' } },
      { isNew: true },
    );

    expect(errors).toContain('This provider ID would reuse a credential reference owned by another provider.');
  });

  test('preserves a saved explicit credential reference but rejects changing it', () => {
    expect(validateProviderIdentity(
      { id: 'legacy', apiKeyEnv: 'SHARED_KEY' },
      { legacy: { apiKeyEnv: 'SHARED_KEY' } },
      { isNew: false, originalId: 'legacy', persistedCredentialRef: 'SHARED_KEY' },
    )).toEqual([]);
    expect(validateProviderIdentity(
      { id: 'legacy', apiKeyEnv: 'LEGACY_API_KEY' },
      { legacy: { apiKeyEnv: 'SHARED_KEY' } },
      { isNew: false, originalId: 'legacy', persistedCredentialRef: 'SHARED_KEY' },
    )).toContain('The saved provider credential reference cannot be changed.');
  });
});

describe('draft conflict and discovery guards', () => {
  test('only marks a dirty draft conflicted when remote revision or data changes', () => {
    expect(hasDraftConflict({
      dirty: false,
      observedRevision: 1,
      remoteRevision: 2,
      observedFingerprint: 'a',
      remoteFingerprint: 'b',
    })).toBe(false);
    expect(hasDraftConflict({
      dirty: true,
      observedRevision: 1,
      remoteRevision: 1,
      observedFingerprint: 'a',
      remoteFingerprint: 'b',
    })).toBe(true);
    expect(hasDraftConflict({
      dirty: true,
      observedRevision: 1,
      remoteRevision: 1,
      observedFingerprint: 'a',
      remoteFingerprint: 'a',
    })).toBe(false);
  });

  test('changes discovery identity when endpoint or key changes and rejects stale generations', () => {
    const draft = validDraft();
    const first = discoveryFingerprint(draft, 'first-secret');
    expect(discoveryFingerprint({ ...draft, baseURL: `${draft.baseURL}/other` }, 'first-secret')).not.toBe(first);
    expect(discoveryFingerprint(draft, 'second-secret')).not.toBe(first);
    expect(isCurrentDiscoveryResult(first, first, 3, 3)).toBe(true);
    expect(isCurrentDiscoveryResult(first, first, 3, 4)).toBe(false);
    expect(isCurrentDiscoveryResult(first, discoveryFingerprint(draft, 'second-secret'), 3, 3)).toBe(false);
  });
});

describe('provider delete default decision', () => {
  const groups = [
    {
      id: 'custom-relay',
      name: 'Custom relay',
      routable: true,
      models: [{ id: 'current-model', name: 'Current model', routable: true }],
    },
    {
      id: 'disabled-relay',
      name: 'Disabled relay',
      routable: false,
      models: [{ id: 'advertised-model', name: 'Advertised model', routable: true }],
    },
    {
      id: 'unknown-relay',
      name: 'Unknown relay',
      models: [{ id: 'unknown-model', name: 'Unknown model', routable: true }],
    },
    {
      id: 'other-relay',
      name: 'Other relay',
      routable: true,
      models: [
        { id: 'disabled-model', name: 'Disabled model', routable: false },
        { id: 'text-model', name: 'Text model', routable: true },
      ],
    },
  ];

  test('keeps a default that belongs elsewhere or is not configured', () => {
    expect(decideDefaultAfterProviderDelete(
      'custom-relay',
      { provider: 'other-relay', model: 'text-model' },
      [],
    )).toEqual({ kind: 'keep' });
    expect(decideDefaultAfterProviderDelete('custom-relay', undefined, [])).toEqual({ kind: 'keep' });
  });

  test('replaces the deleted default with the first explicitly routable remaining route', () => {
    expect(decideDefaultAfterProviderDelete(
      'custom-relay',
      { provider: 'custom-relay', model: 'current-model' },
      groups,
    )).toEqual({
      kind: 'replace',
      selection: { provider: 'other-relay', model: 'text-model' },
      notice: 'The default was changed to “Text model” from “Other relay”.',
    });
  });

  test('blocks with actionable notice when no other route is explicitly routable', () => {
    expect(decideDefaultAfterProviderDelete(
      'custom-relay',
      { provider: 'custom-relay', model: 'current-model' },
      groups.slice(0, 3),
    )).toEqual({
      kind: 'block',
      notice: 'This provider is the current default, and no other routable model is available. Configure another routable provider and model, then try again.',
    });
  });
});
