import type { Context } from '@deepseek-ai/cordis';
import type { ConnectionHandle } from '@deepseek-ai/dsh-client-connection/client';

import { registerModelSelection } from './model-selection';
import { registerProviderSettings } from './provider-settings';

export const inject = ['slots', 'connection'] as const;

export function apply(ctx: Context): void {
  const connection = ctx.get('connection') as ConnectionHandle;
  registerProviderSettings(ctx, connection.api);
  registerModelSelection(ctx, connection.api);
}
