// Run cargo in the Rust workspace with the engine build environment, e.g.
// `node scripts/cargo.mjs test --workspace`.
import { root, run, rustEnv } from './rust-env.mjs';

const env = await rustEnv();
process.chdir(root);
await run('cargo', [`+${process.env.RUSTUP_TOOLCHAIN || 'stable'}`, ...process.argv.slice(2)], env);
