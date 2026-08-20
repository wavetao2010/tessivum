export const createRequire = (): never => {
  throw new Error('node:module is not available in the browser');
};

export type LoadHookContext = never;
