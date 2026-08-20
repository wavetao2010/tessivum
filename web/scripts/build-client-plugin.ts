import { mkdir } from 'node:fs/promises';
import { resolve } from 'node:path';

declare const Bun: {
  build(options: Record<string, unknown>): Promise<{ success: boolean; outputs: Array<{ path: string }>; logs: Array<{ level?: string; message?: string }> }>;
  file(path: string): { text(): Promise<string> };
  write(path: string, content: string): Promise<number>;
};

const root = resolve(import.meta.dir, '..');
const pkg = resolve(root, 'packages/client-config');
const outdir = resolve(pkg, 'lib');
const result = await Bun.build({
  entrypoints: [resolve(pkg, 'src/client.ts')],
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
await mkdir(outdir, { recursive: true });
await Bun.write(resolve(outdir, 'client.js'), bundle);
console.info(`built @tessivum/client-config (${bundle.length} bytes)`);
