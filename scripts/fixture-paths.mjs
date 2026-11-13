// Use the Rust boundary before any fixture directory creation or removal.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

export function developmentRoot(root, candidate = process.env.HER_TEST_CLI ?? process.env.HER_PARITY_CANDIDATE) {
  const candidateIndex = process.argv.indexOf('--candidate');
  candidate ??= candidateIndex >= 0 ? process.argv[candidateIndex + 1] : undefined;
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const repo = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
  const cli = process.env.HER_TEST_CLI ?? (candidate
    ? path.join(path.dirname(candidate), `her-cli${suffix}`)
    : path.join(process.env.CARGO_TARGET_DIR ?? path.join(repo, 'her/target'), 'debug', `her-cli${suffix}`));
  if (!fs.existsSync(cli)) throw Error('Set HER_TEST_CLI to the built her-cli before allocating fixture roots');
  const env = { ...process.env };
  delete env.HER_RUNTIME_ROOT;
  const selected = path.isAbsolute(root) ? root : process.cwd() + path.sep + root;
  const result = spawnSync(cli, ['install-root', 'check-development', '--runtime-root', selected],
    { env, windowsHide: true, encoding: 'utf8', timeout: 15000 });
  if (result.error || result.status !== 0) throw Error(`fixture root refused: ${result.error ?? result.stderr}`);
  return root;
}
