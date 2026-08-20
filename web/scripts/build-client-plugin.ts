type BuildResult = {
  success: boolean;
  outputs: Array<{ path: string }>;
  logs: Array<{ level?: string; message?: string }>;
};

type BunApi = {
  build(options: Record<string, unknown>): Promise<BuildResult>;
  file(path: string): { text(): Promise<string> };
  mkdir(path: string, options?: { recursive?: boolean }): Promise<void>;
  write(path: string, content: string): Promise<number>;
};

declare const Bun: BunApi;

const root = new URL('../', import.meta.url);
const pkg = new URL('packages/client-config/', root);
const outdir = new URL('lib/', pkg).pathname;
const packageSource = new URL('src/client.ts', pkg).pathname;
await Bun.mkdir(outdir, { recursive: true });

const result = await Bun.build({
  entrypoints: [packageSource],
  outdir,
  naming: 'client.js',
  format: 'cjs',
  target: 'browser',
  minify: true,
  external: [
    'react',
    'react-dom',
    'react/jsx-runtime',
    'react/jsx-dev-runtime',
    '@deepseek-ai/cordis',
    '@deepseek-ai/dsh-client-connection/client',
  ],
});

if (!result.success || result.outputs.length !== 1) {
  const details = result.logs.map((log) => `${log.level ?? 'error'}: ${log.message ?? ''}`).join('\n');
  throw new Error(`client-config build failed${details ? `\n${details}` : ''}`);
}

const source = await Bun.file(result.outputs[0].path).text();
const bundle = `window.__ModuleLoader__.load({id:"@tessivum/client-config",factory:function(require){const module={exports:{}};const exports=module.exports;${source}\nreturn module.exports;}});\n`;
await Bun.write(`${outdir}/client.js`, bundle);
console.info(`built @tessivum/client-config (${bundle.length} bytes)`);
