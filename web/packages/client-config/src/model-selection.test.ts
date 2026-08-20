import { describe, expect, test } from 'bun:test';
import {
  currentModelLabel,
  groupModelCatalog,
  hasRoutableModels,
  modelRouteValue,
  parseSessionModelsResponse,
  selectionForValue,
  type ModelCatalog,
} from './model-selection';

const catalog: ModelCatalog = {
  current: { provider: 'openai', model: 'gpt-5', reasoningEffort: 'high' },
  routable: true,
  groups: [
    {
      id: 'openai',
      name: 'OpenAI',
      routable: true,
      models: [
        { id: 'gpt-5', name: 'GPT-5', routable: true, reasoning: { defaultEffort: 'medium' } },
        { id: 'gpt-5-mini', name: 'GPT-5 mini', routable: true },
      ],
    },
    {
      id: 'local',
      name: 'Local',
      routable: true,
      models: [{ id: 'reasoner', name: 'Reasoner', routable: true, reasoning: { defaultEffort: 'low' } }],
    },
  ],
};

describe('parseSessionModelsResponse', () => {
  test('accepts the enriched host catalog without stripping routability', () => {
    const value = {
      current: { provider: 'openai', model: 'gpt-5' },
      routable: true,
      groups: [{
        id: 'openai',
        name: 'OpenAI',
        credentialConfigured: true,
        routable: true,
        models: [{
          id: 'gpt-5',
          name: 'GPT-5',
          provider: 'openai',
          inputModalities: ['text'],
          routable: true,
        }],
      }],
    };
    const parsed = parseSessionModelsResponse({
      type: 'server-response',
      rpcId: 'rpc-1',
      result: { ok: true, value },
    }, 'rpc-1');

    expect(parsed).toBe(value);
    expect(parsed.groups[0]!.routable).toBe(true);
    expect(parsed.groups[0]!.models[0]!.routable).toBe(true);
  });

  test('rejects mismatched envelopes and malformed catalog readiness', () => {
    const result = { ok: true, value: catalog };
    const malformed = [
      { type: 'client-response', rpcId: 'rpc-1', result },
      { type: 'server-response', rpcId: 'other-rpc', result },
      { type: 'server-response', rpcId: 'rpc-1', result: { value: catalog } },
      {
        type: 'server-response',
        rpcId: 'rpc-1',
        result: {
          ok: true,
          value: {
            ...catalog,
            groups: [{
              ...catalog.groups[0]!,
              models: [{ id: 'gpt-5', name: 'GPT-5' }],
            }],
          },
        },
      },
    ];

    for (const envelope of malformed) {
      expect(() => parseSessionModelsResponse(envelope, 'rpc-1')).toThrow('Invalid session models response.');
    }
  });

  test('surfaces bounded server errors and handles malformed error results', () => {
    expect(() => parseSessionModelsResponse({
      type: 'server-response',
      rpcId: 'rpc-1',
      result: { ok: false, error: { code: 'not_found', message: 'Models unavailable' } },
    }, 'rpc-1')).toThrow('Models unavailable');
    expect(() => parseSessionModelsResponse({
      type: 'server-response',
      rpcId: 'rpc-1',
      result: { ok: false, error: null },
    }, 'rpc-1')).toThrow('Unable to load models');
  });
});

describe('model catalog helpers', () => {
  test('groups models by provider and keeps complete route selections', () => {
    const groups = groupModelCatalog(catalog);

    expect(groups.map((group) => [group.label, group.options.map((option) => option.label)])).toEqual([
      ['OpenAI', ['GPT-5', 'GPT-5 mini']],
      ['Local', ['Reasoner']],
    ]);
    expect(groups[0]!.options[0]!.selection.reasoningEffort).toBe('high');
    expect(selectionForValue(groups, modelRouteValue('local', 'reasoner'))).toEqual({
      provider: 'local',
      model: 'reasoner',
      reasoningEffort: 'low',
    });
  });

  test('disables routes when either provider or model is non-routable', () => {
    const restricted: ModelCatalog = {
      current: { provider: 'mixed', model: 'enabled-model' },
      routable: true,
      groups: [
        {
          id: 'disabled-provider',
          name: 'Disabled provider',
          routable: false,
          models: [{ id: 'advertised-model', name: 'Advertised model', routable: true }],
        },
        {
          id: 'mixed',
          name: 'Mixed',
          routable: true,
          models: [
            { id: 'disabled-model', name: 'Disabled model', routable: false },
            { id: 'enabled-model', name: 'Enabled model', routable: true },
          ],
        },
      ],
    };

    const groups = groupModelCatalog(restricted);
    expect(groups.map((group) => group.options.map((option) => option.routable))).toEqual([
      [false],
      [false, true],
    ]);
    expect(selectionForValue(groups, modelRouteValue('disabled-provider', 'advertised-model'))).toBeUndefined();
    expect(selectionForValue(groups, modelRouteValue('mixed', 'disabled-model'))).toBeUndefined();
    expect(selectionForValue(groups, modelRouteValue('mixed', 'enabled-model'))).toEqual(restricted.current);
    expect(hasRoutableModels(restricted)).toBe(true);
  });

  test('preserves and labels the current route when the directory omits it', () => {
    const stale: ModelCatalog = {
      current: { provider: 'retired-provider', model: 'legacy-model' },
      routable: true,
      groups: [{
        id: 'openai',
        name: 'OpenAI',
        routable: true,
        models: [{ id: 'gpt-5', name: 'GPT-5', routable: true }],
      }],
    };

    const fallback = groupModelCatalog(stale)[0]!.options[0]!;
    expect(fallback).toMatchObject({
      label: 'legacy-model (current)',
      current: true,
      routable: false,
      selection: stale.current,
    });
    expect(selectionForValue(groupModelCatalog(stale), fallback.value)).toBeUndefined();
    expect(currentModelLabel(stale)).toBe('legacy-model — retired-provider');
  });

  test('reports availability only when the catalog includes a selectable option', () => {
    const unavailable: ModelCatalog = {
      current: { provider: 'missing', model: 'missing' },
      routable: true,
      groups: [],
    };
    const group: ModelCatalog['groups'][number] = {
      id: 'openai',
      name: 'OpenAI',
      routable: true,
      models: [{ id: 'gpt-5', name: 'GPT-5', routable: true }],
    };

    expect(hasRoutableModels(unavailable)).toBe(false);
    expect(hasRoutableModels({ ...unavailable, groups: [{ ...group, routable: false }] })).toBe(false);
    expect(hasRoutableModels({
      ...unavailable,
      groups: [{ ...group, models: [{ ...group.models[0]!, routable: false }] }],
    })).toBe(false);
    expect(hasRoutableModels({ ...unavailable, groups: [group] })).toBe(true);
  });
});
