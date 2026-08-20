import { describe, expect, test } from 'bun:test';
import {
  deriveCredentialRef,
  normalizeCatalogReadiness,
  validateApiKey,
  validateProviderDraft,
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
