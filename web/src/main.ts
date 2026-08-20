import './base.css';
import type { AppWebEntry as AppWebEntryClass } from '@deepseek-ai/dsh-client-web';

declare global {
  interface Window {
    __DSH_BOOT__?: unknown;
  }
}

type BootEntry = {
  id: string;
  url: string;
  rev: string;
  inject?: string[];
  immediately?: boolean;
};

type BootManifest = {
  rev: string;
  modules: Pick<BootEntry, 'id' | 'url' | 'rev'>[];
  plugins: Required<Pick<BootEntry, 'id' | 'inject' | 'immediately'>>[];
};

type ModuleHandoff = {
  id: string;
  factory: (require: (specifier: string) => unknown) => Record<string, unknown>;
};

type ModuleRecord = {
  id: string;
  surface: unknown;
  styles: string[];
  edges: Set<string>;
};

type ModuleWindow = Window & {
  __DSH_MODULES__?: ClientModuleSystem;
  __ModuleLoader__?: { load(handoff: ModuleHandoff): void };
};

const moduleWindow = window as ModuleWindow;

function moduleId(id: string): string {
  return id.startsWith('@') ? id.slice(1).replace('/', '~') : id;
}

// ponytail: delete this shim when the published client module is runtime-safe for browser ESM.
export function createRequire(): never {
  throw new Error('node:module is not available in the browser');
}

export function parseBootManifest(wire: unknown): BootManifest {
  if (wire === null || typeof wire !== 'object' || Array.isArray(wire)) {
    throw new TypeError('window.__DSH_BOOT__ is missing or not an object');
  }

  const { entries, rev } = wire as Record<string, unknown>;
  if (typeof rev !== 'string' || !Array.isArray(entries)) {
    throw new TypeError('window.__DSH_BOOT__ must contain string rev and entries');
  }

  const modules: BootManifest['modules'] = [];
  const plugins: BootManifest['plugins'] = [];
  for (const entry of entries) {
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
      throw new TypeError('window.__DSH_BOOT__.entries must contain objects');
    }

    const { id, immediately, inject, url, rev: entryRev } = entry as Record<string, unknown>;
    if (typeof id !== 'string' || typeof url !== 'string' || typeof entryRev !== 'string') {
      throw new TypeError('window.__DSH_BOOT__.entries must contain string id, url, and rev');
    }
    if (inject !== undefined && (!Array.isArray(inject) || inject.some((value) => typeof value !== 'string'))) {
      throw new TypeError(`window.__DSH_BOOT__.entries[${id}].inject must be a string array`);
    }
    if (immediately !== undefined && typeof immediately !== 'boolean') {
      throw new TypeError(`window.__DSH_BOOT__.entries[${id}].immediately must be a boolean`);
    }

    modules.push({ id, url, rev: entryRev });
    plugins.push({ id, inject: inject === undefined ? [] : [...inject], immediately: immediately === true });
  }

  return { rev, modules, plugins };
}

export class ClientModuleSystem {
  readonly version = 'client' as const;
  readonly loadCache = new Map<string, ModuleRecord>();
  private readonly seed: Map<string, unknown>;
  private readonly statics = new Map<string, unknown>();
  private readonly factories = new Map<string, ModuleHandoff['factory']>();
  private readonly pendingArrival = new Map<string, Promise<void>>();
  private readonly materializing = new Set<string>();
  private readonly graphRows = new Map<string, BootManifest['modules'][number]>();
  private readonly loadBundle: (url: string) => Promise<void>;

  constructor(options: {
    modules: BootManifest['modules'];
    staticModules: Record<string, unknown>;
    loadBundle?: (url: string) => Promise<void>;
  }) {
    this.seed = new Map(Object.entries(options.staticModules));
    this.loadBundle = options.loadBundle ?? ClientModuleSystem.loadBundle;
    for (const row of options.modules) {
      if (this.graphRows.has(row.id)) throw new Error(`client-modules: duplicate graph entry "${row.id}"`);
      this.graphRows.set(row.id, row);
    }
    if (moduleWindow.__ModuleLoader__ !== undefined) {
      throw new Error('client-modules: window.__ModuleLoader__ already installed (double boot?)');
    }
    moduleWindow.__ModuleLoader__ = { load: (handoff) => {
      const id = this.graphRows.has(handoff.id) ? handoff.id : moduleId(handoff.id);
      if (!this.graphRows.has(id)) throw new Error(`client-modules: unknown factory registration "${handoff.id}"`);
      if (this.factories.has(id)) throw new Error(`client-modules: duplicate factory registration for "${id}"`);
      this.factories.set(id, handoff.factory);
    } };
  }

  async import(specifier: string): Promise<unknown> {
    const seeded = this.seed.get(specifier);
    if (seeded !== undefined) return seeded;
    const staticModule = this.statics.get(specifier);
    if (staticModule !== undefined) {
      this.loadCache.set(specifier, { id: specifier, surface: staticModule, styles: [], edges: new Set() });
      return staticModule;
    }
    const id = moduleId(specifier);
    const cached = this.loadCache.get(id);
    if (cached !== undefined) return cached.surface;
    if (!this.factories.has(id)) {
      const row = this.graphRows.get(id);
      if (row === undefined) throw new Error(`client-modules: cannot resolve "${specifier}"`);
      await this.arrive(row);
    }
    return this.materialize(id).surface;
  }

  registerStatic(id: string, module: unknown): void {
    if (this.statics.has(id)) throw new Error(`client-modules: shell-own module "${id}" registered twice`);
    this.statics.set(id, module);
  }

  async prefetch(id: string): Promise<void> {
    if (this.statics.has(id)) return;
    const key = moduleId(id);
    const row = this.graphRows.get(key);
    if (row === undefined) throw new Error(`client-modules: prefetch("${id}") — not a graph entry`);
    await this.arrive(row);
  }

  invalidate(id: string): void {
    const key = moduleId(id);
    this.factories.delete(key);
    this.loadCache.delete(key);
  }

  private async arrive(row: BootManifest['modules'][number]): Promise<void> {
    const pending = this.pendingArrival.get(row.id);
    if (pending !== undefined) return pending;
    if (this.factories.has(row.id)) return;
    const task = this.loadBundle(row.url).then(() => {
      if (!this.factories.has(row.id)) throw new Error(`client-modules: bundle ${row.url} did not register "${row.id}"`);
    }).finally(() => {
      this.pendingArrival.delete(row.id);
    });
    this.pendingArrival.set(row.id, task);
    await task;
  }

  private materialize(id: string): ModuleRecord {
    const cached = this.loadCache.get(id);
    if (cached !== undefined) return cached;
    const factory = this.factories.get(id);
    if (factory === undefined) throw new Error(`client-modules: no registered factory for "${id}"`);
    if (this.materializing.has(id)) throw new Error(`client-modules: require cycle through "${id}"`);

    this.materializing.add(id);
    try {
      const edges = new Set<string>();
      const record = { id, surface: factory(this.require(edges)), styles: [], edges };
      this.loadCache.set(id, record);
      return record;
    } finally {
      this.materializing.delete(id);
    }
  }

  private require(edges: Set<string>): (specifier: string) => unknown {
    return (specifier) => {
      edges.add(specifier);
      const seeded = this.seed.get(specifier);
      if (seeded !== undefined) return seeded;
      const staticModule = this.statics.get(specifier);
      if (staticModule !== undefined) return staticModule;
      const id = moduleId(specifier.endsWith('/client') ? specifier.slice(0, -7) : specifier);
      const cached = this.loadCache.get(id);
      if (cached !== undefined) return cached.surface;
      if (this.factories.has(id)) return this.materialize(id).surface;
      throw new Error(`client-modules: require("${specifier}") missed the module table`);
    };
  }

  private static loadBundle(url: string): Promise<void> {
    const { promise, reject, resolve } = Promise.withResolvers<void>();
    const script = document.createElement('script');
    script.async = true;
    script.src = url;
    script.addEventListener('load', () => {
      script.remove();
      resolve();
    }, { once: true });
    script.addEventListener('error', () => {
      script.remove();
      reject(new Error(`client-modules: bundle script ${url} failed to load`));
    }, { once: true });
    document.head.append(script);
    return promise;
  }
}

export function apply(ctx: { reflect: { provide(name: string, value: unknown): void } }): void {
  if (moduleWindow.__DSH_MODULES__ === undefined) {
    throw new Error('client-modules: window.__DSH_MODULES__ missing');
  }
  ctx.reflect.provide('modules', moduleWindow.__DSH_MODULES__);
}

type BootGraph = { rev: string; entries: unknown[] };

function isBootGraph(value: unknown): value is BootGraph {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    && typeof (value as Record<string, unknown>).rev === 'string'
    && Array.isArray((value as Record<string, unknown>).entries);
}

function pluginBundle(entry: unknown): Record<string, unknown> {
  if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
    throw new TypeError('window.__DSH_BOOT__.entries must contain objects');
  }

  const { id, rev, url } = entry as Record<string, unknown>;
  if (typeof id !== 'string' || !/^[A-Za-z0-9._~-]+$/.test(id) || typeof rev !== 'string' || typeof url !== 'string') {
    throw new TypeError('window.__DSH_BOOT__.entries must contain id, rev, and url strings');
  }

  const bundle = new URL(url, window.location.origin);
  if (bundle.origin !== window.location.origin || bundle.pathname !== `/plugins/${id}/client.js` || bundle.hash) {
    throw new TypeError('window.__DSH_BOOT__.entries must target same-origin plugin bundles');
  }

  bundle.search = new URLSearchParams({ rev }).toString();
  return { ...entry, url: `${bundle.pathname}${bundle.search}` };
}

async function start(): Promise<void> {
  const boot = window.__DSH_BOOT__;
  if (!isBootGraph(boot)) throw new TypeError('window.__DSH_BOOT__ must be an object with rev and entries');
  window.__DSH_BOOT__ = { ...boot, entries: boot.entries.map(pluginBundle) };

  const root = document.getElementById('root');
  if (!(root instanceof HTMLElement)) throw new Error('Missing required #root element');

  // The published shell self-registers during evaluation, before its real sink exists.
  const { AppWebEntry } = await import('@deepseek-ai/dsh-client-web');
  void new (AppWebEntry as typeof AppWebEntryClass)(root).run();
}

void start();
