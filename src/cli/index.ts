import { Program } from 'commander';
import { VERB } from './verbs';
import { EXECUTOR } from './verbs/context_fetch';

const cli = new Program();

// Register verbs here
cli.register(VERB, EXECUTOR);

export default cli;
