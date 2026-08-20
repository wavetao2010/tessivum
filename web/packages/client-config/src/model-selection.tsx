import * as React from 'react';
import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react';
import type { ChangeEvent } from 'react';
import type { Context } from '@deepseek-ai/cordis';
import type { IApiClient, SessionId } from '@deepseek-ai/dsh-client-connection/client';
import type {} from '@deepseek-ai/dsh-client-runtime/client';
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client';

export interface CatalogSelection {
  provider: string;
  model: string;
  reasoningEffort?: string;
}

export interface CatalogModel {
  id: string;
  name: string;
  routable?: boolean;
  reasoning?: { defaultEffort?: string };
}

export interface CatalogProvider {
  id: string;
  name: string;
  routable?: boolean;
  models: readonly CatalogModel[];
}

export interface ModelCatalog {
  current: CatalogSelection;
  routable: boolean;
  groups: readonly CatalogProvider[];
}

function isCatalogSelection(value: unknown): value is CatalogSelection {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && 'provider' in value && typeof value.provider === 'string'
    && 'model' in value && typeof value.model === 'string'
    && (!('reasoningEffort' in value)
      || value.reasoningEffort === undefined
      || typeof value.reasoningEffort === 'string');
}

function isCatalogModel(value: unknown): value is CatalogModel {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && 'id' in value && typeof value.id === 'string'
    && 'name' in value && typeof value.name === 'string'
    && 'routable' in value && typeof value.routable === 'boolean';
}

function isCatalogProvider(value: unknown): value is CatalogProvider {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && 'id' in value && typeof value.id === 'string'
    && 'name' in value && typeof value.name === 'string'
    && 'routable' in value && typeof value.routable === 'boolean'
    && 'models' in value && Array.isArray(value.models)
    && value.models.every(isCatalogModel);
}

function isModelCatalog(value: unknown): value is ModelCatalog {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    && 'current' in value && isCatalogSelection(value.current)
    && 'routable' in value && typeof value.routable === 'boolean'
    && 'groups' in value && Array.isArray(value.groups)
    && value.groups.every(isCatalogProvider);
}

export function parseSessionModelsResponse(envelope: unknown, rpcId: string): ModelCatalog {
  if (
    typeof envelope !== 'object'
    || envelope === null
    || Array.isArray(envelope)
    || !('type' in envelope)
    || envelope.type !== 'server-response'
    || !('rpcId' in envelope)
    || envelope.rpcId !== rpcId
    || !('result' in envelope)
  ) {
    throw new Error('Invalid session models response.');
  }
  const result = envelope.result;
  if (typeof result !== 'object' || result === null || Array.isArray(result) || !('ok' in result)) {
    throw new Error('Invalid session models response.');
  }
  if (result.ok === false) {
    const error = 'error' in result ? result.error : undefined;
    const message = typeof error === 'object'
      && error !== null
      && !Array.isArray(error)
      && 'message' in error
      && typeof error.message === 'string'
      && error.message
      ? error.message
      : 'Unable to load models';
    throw new Error(message);
  }
  if (result.ok !== true || !('value' in result) || !isModelCatalog(result.value)) {
    throw new Error('Invalid session models response.');
  }
  return result.value;
}

async function loadSessionModels(sessionId: SessionId, signal?: AbortSignal): Promise<ModelCatalog> {
  const rpcId = crypto.randomUUID();
  const response = await fetch('/api/session.models', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      type: 'client-request',
      rpcId,
      method: 'session.models',
      payload: { sessionId },
    }),
    signal,
  });
  if (!response.ok) throw new Error(`Unable to load models (HTTP ${response.status})`);
  return parseSessionModelsResponse(await response.json(), rpcId);
}

export interface CatalogOption {
  value: string;
  label: string;
  selection: CatalogSelection;
  current: boolean;
  routable: boolean;
}

export interface CatalogGroup {
  provider: string;
  label: string;
  options: readonly CatalogOption[];
}

export function modelRouteValue(provider: string, model: string): string {
  return JSON.stringify([provider, model]);
}

function sameRoute(left: CatalogSelection, right: CatalogSelection): boolean {
  return left.provider === right.provider && left.model === right.model;
}

export function groupModelCatalog(catalog: ModelCatalog): CatalogGroup[] {
  const groups: CatalogGroup[] = catalog.groups.map((provider) => ({
    provider: provider.id,
    label: provider.name,
    options: provider.models.map((model) => {
      const selection: CatalogSelection = {
        provider: provider.id,
        model: model.id,
        ...(sameRoute(catalog.current, { provider: provider.id, model: model.id })
          ? catalog.current.reasoningEffort === undefined
            ? {}
            : { reasoningEffort: catalog.current.reasoningEffort }
          : model.reasoning?.defaultEffort === undefined
            ? {}
            : { reasoningEffort: model.reasoning.defaultEffort }),
      };
      return {
        value: modelRouteValue(provider.id, model.id),
        label: model.name,
        selection,
        current: sameRoute(catalog.current, selection),
        routable: provider.routable === true && model.routable === true,
      };
    }),
  }));

  if (!groups.some((group) => group.options.some((option) => option.current))) {
    const fallback: CatalogOption = {
      value: modelRouteValue(catalog.current.provider, catalog.current.model),
      label: `${catalog.current.model} (current)`,
      selection: catalog.current,
      current: true,
      routable: false,
    };
    const providerIndex = groups.findIndex((group) => group.provider === catalog.current.provider);
    if (providerIndex === -1) {
      groups.unshift({ provider: catalog.current.provider, label: catalog.current.provider, options: [fallback] });
    } else {
      const provider = groups[providerIndex]!;
      groups[providerIndex] = { ...provider, options: [fallback, ...provider.options] };
    }
  }

  return groups.filter((group) => group.options.length > 0);
}

export function hasRoutableModels(catalog: ModelCatalog): boolean {
  return catalog.groups.some((provider) =>
    provider.routable === true && provider.models.some((model) => model.routable === true));
}

export function currentModelLabel(catalog: ModelCatalog): string {
  for (const provider of catalog.groups) {
    if (provider.id !== catalog.current.provider) continue;
    const model = provider.models.find((candidate) => candidate.id === catalog.current.model);
    return `${model?.name ?? catalog.current.model} — ${provider.name}`;
  }
  return `${catalog.current.model} — ${catalog.current.provider}`;
}

export function selectionForValue(groups: readonly CatalogGroup[], value: string): CatalogSelection | undefined {
  for (const group of groups) {
    const option = group.options.find((candidate) => candidate.routable && candidate.value === value);
    if (option !== undefined) return option.selection;
  }
  return undefined;
}

const selectorStyles = `
.tessivum-model-selector { display: inline-flex; align-items: center; gap: 4px; min-width: 0; }
.tessivum-model-selector select {
  box-sizing: border-box;
  min-width: 0;
  max-width: 220px;
  height: 28px;
  padding: 0 8px;
  color: var(--dsw-alias-label-secondary);
  background: transparent;
  border: 0;
  border-radius: 24px;
  font-family: inherit;
  font-size: 13px;
  font-weight: 500;
  line-height: 20px;
  text-overflow: ellipsis;
  cursor: pointer;
}
.tessivum-model-selector select:hover:not(:disabled) { background: var(--dsw-alias-interactive-bg-hover); }
.tessivum-model-selector select:focus-visible {
  outline: 2px solid var(--dsw-alias-border-l3);
  outline-offset: 0;
}
.tessivum-model-selector select:disabled {
  color: var(--dsw-alias-label-dimmed);
  cursor: not-allowed;
}
.tessivum-model-selector__error {
  color: var(--dsw-alias-state-error-primary);
  font-size: 12px;
  line-height: 20px;
}
.tessivum-model-selector__sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}
`;

interface ModelSelectorProps {
  api: IApiClient;
  ctx: Context;
  sessionId: SessionId;
  locked: boolean;
}


function ModelSelector({ api, ctx, sessionId, locked }: ModelSelectorProps) {
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null);
  const [refreshing, setRefreshing] = useState(true);
  const [selecting, setSelecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const catalogRef = useRef<ModelCatalog | null>(null);
  const loadGeneration = useRef(0);
  const loadAbort = useRef<AbortController | null>(null);
  const alive = useRef(true);
  const selectingRef = useRef(false);
  const refreshQueued = useRef(false);
  const errorId = useId();

  const publishCatalog = useCallback((next: ModelCatalog) => {
    catalogRef.current = next;
    setCatalog(next);
  }, []);

  const refresh = useCallback(async () => {
    if (selectingRef.current) {
      refreshQueued.current = true;
      return;
    }

    const generation = ++loadGeneration.current;
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    setRefreshing(true);
    try {
      const next = await loadSessionModels(sessionId, controller.signal);
      if (!alive.current || generation !== loadGeneration.current) return;
      publishCatalog(next);
      setError(null);
    } catch (cause) {
      if (controller.signal.aborted) return;
      if (alive.current && generation === loadGeneration.current) {
        setError(cause instanceof Error && cause.message ? cause.message : 'Unable to load models');
      }
    } finally {
      if (loadAbort.current === controller) loadAbort.current = null;
      if (alive.current && generation === loadGeneration.current) setRefreshing(false);
    }
  }, [publishCatalog, sessionId]);

  useEffect(() => {
    alive.current = true;
    const disposeModels = ctx.on('models/changed', () => void refresh());
    const disposeSettings = ctx.on('settings/changed', () => void refresh());
    const disposeCredentials = ctx.on('credentials/changed', () => void refresh());
    const disposeConnection = ctx.on('connection/reset', () => void refresh());
    void refresh();
    return () => {
      alive.current = false;
      loadAbort.current?.abort();
      loadAbort.current = null;
      ++loadGeneration.current;
      disposeConnection();
      disposeCredentials();
      disposeSettings();
      disposeModels();
    };
  }, [ctx, refresh]);

  const groups = useMemo(() => catalog === null ? [] : groupModelCatalog(catalog), [catalog]);
  const loading = catalog === null && refreshing;
  const disabled = locked || loading || selecting || catalog === null || !hasRoutableModels(catalog);
  const value = catalog === null ? '' : modelRouteValue(catalog.current.provider, catalog.current.model);
  const label = catalog === null ? 'Model selector' : `Model: ${currentModelLabel(catalog)}`;

  const choose = async (event: ChangeEvent<HTMLSelectElement>) => {
    const current = catalogRef.current;
    const selection = selectionForValue(groups, event.currentTarget.value);
    if (current === null || selection === undefined || sameRoute(current.current, selection)) return;

    ++loadGeneration.current;
    loadAbort.current?.abort();
    loadAbort.current = null;
    setRefreshing(false);
    selectingRef.current = true;
    setSelecting(true);
    setError(null);
    try {
      const { result } = await api.sessions.selectModel({ sessionId, ...selection });
      if (!alive.current) return;
      if (result.ok) {
        publishCatalog({ ...catalogRef.current!, current: result.value.selected, routable: true });
      } else {
        setError(result.error.message);
      }
    } catch (cause) {
      if (alive.current) {
        setError(cause instanceof Error && cause.message ? cause.message : 'Unable to select model');
      }
    } finally {
      selectingRef.current = false;
      if (alive.current) {
        setSelecting(false);
        if (refreshQueued.current) {
          refreshQueued.current = false;
          void refresh();
        }
      }
    }
  };

  return (
    <span className="tessivum-model-selector">
      <style>{selectorStyles}</style>
      <select
        aria-busy={refreshing || selecting}
        aria-describedby={error === null ? undefined : errorId}
        aria-label={label}
        disabled={disabled}
        onChange={(event) => void choose(event)}
        onFocus={() => void refresh()}
        title={error ?? label}
        value={value}
      >
        {catalog === null ? (
          <option value="">{loading ? 'Loading models…' : 'Models unavailable'}</option>
        ) : groups.map((group) => (
          <optgroup key={group.provider} label={group.label}>
            {group.options.map((option) => (
              <option disabled={!option.routable} key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </optgroup>
        ))}
      </select>
      {error === null ? null : (
        <span className="tessivum-model-selector__error" id={errorId} role="status" title={error}>
          <span aria-hidden="true">!</span>
          <span className="tessivum-model-selector__sr-only">{error}</span>
        </span>
      )}
    </span>
  );
}

export function registerModelSelection(ctx: Context, api: IApiClient): void {
  ctx.slots.inject('conversation.input.model', () => ctx.slots.register({
    name: 'conversation.input.model',
    inject: () => ({ api, ctx }),
  }, ModelSelector));
}
