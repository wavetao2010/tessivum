import { describe, expect, test } from 'bun:test';
import {
  deriveCredentialRef,
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

describe('deriveCredentialRef', () => {
  test('uppercases the provider id and replaces every non-alphanumeric character', () => {
    expect(deriveCredentialRef('openai-responses')).toBe('OPENAI_RESPONSES_API_KEY');
    expect(deriveCredentialRef('relay.eu/v2')).toBe('RELAY_EU_V2_API_KEY');
    expect(deriveCredentialRef('a--b')).toBe('A__B_API_KEY');
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
