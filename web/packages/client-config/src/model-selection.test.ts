import { describe, expect, test } from 'bun:test';
import {
  currentModelLabel,
  groupModelCatalog,
  hasRoutableModels,
  modelRouteValue,
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
      models: [
        { id: 'gpt-5', name: 'GPT-5', reasoning: { defaultEffort: 'medium' } },
        { id: 'gpt-5-mini', name: 'GPT-5 mini' },
      ],
    },
    {
      id: 'local',
      name: 'Local',
      models: [{ id: 'reasoner', name: 'Reasoner', reasoning: { defaultEffort: 'low' } }],
    },
  ],
};

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

  test('preserves and labels the current route when the directory omits it', () => {
    const stale: ModelCatalog = {
      current: { provider: 'retired-provider', model: 'legacy-model' },
      routable: false,
      groups: [{ id: 'openai', name: 'OpenAI', models: [{ id: 'gpt-5', name: 'GPT-5' }] }],
    };

    const fallback = groupModelCatalog(stale)[0]!.options[0]!;
    expect(fallback).toMatchObject({
      label: 'legacy-model (current)',
      current: true,
      routable: false,
      selection: stale.current,
    });
    expect(currentModelLabel(stale)).toBe('legacy-model — retired-provider');
  });

  test('reports availability from advertised routes or a routable current fallback', () => {
    const unavailable: ModelCatalog = {
      current: { provider: 'missing', model: 'missing' },
      routable: false,
      groups: [],
    };

    expect(hasRoutableModels(unavailable)).toBe(false);
    expect(hasRoutableModels({ ...unavailable, routable: true })).toBe(true);
    expect(hasRoutableModels({
      ...unavailable,
      groups: [{ id: 'openai', name: 'OpenAI', models: [{ id: 'gpt-5', name: 'GPT-5' }] }],
    })).toBe(true);
  });
});
