import { Command, Args, Options } from 'commander';

export const VERB = 'context_fetch';

export const COMMAND = 'forge context fetch';

export const ARGS = {
  CONTEXT_ID: 'id',
  CONTEXT_NAME: 'name',
};

export const OPTIONS = {
  VERBOSE: 'verbose',
};

export const EXECUTOR = {
  NAME: 'context_fetch',
  COMMAND: COMMAND,
  ARGS: ARGS,
  OPTIONS: OPTIONS,
  VERB: VERB,
};
