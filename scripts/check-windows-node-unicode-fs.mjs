export const SUPPORTED_NODE_RANGES = Object.freeze([
  '>=22.19.0 <23.0.0',
  '>=24.13.1 <25.0.0',
]);

export function isSupportedWindowsNodeVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (!match) {
    return false;
  }

  const major = Number(match[1]);
  const minor = Number(match[2]);
  const patch = Number(match[3]);

  if (major === 22) {
    return minor > 19 || (minor === 19 && patch >= 0);
  }

  if (major === 24) {
    return minor > 13 || (minor === 13 && patch >= 1);
  }

  return false;
}
