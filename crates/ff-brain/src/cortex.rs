//! Cortex — native code-extraction lobe for the Brain faceted graph.
//!
//! Parses code files (Rust, TypeScript/TSX, JavaScript, Java) with the native
//! `tree-sitter` grammar crates — NO Python, NO external tool. For
//! each file already scanned by `corpus.rs` as a `content:file` brain_vault_nodes
//! row, Cortex extracts symbol nodes and call/import/contains edges into the V117
//! Brain tables (reused wholesale — NO new tables, NO new columns).
//!
//! NODE MODEL
//!   Code symbols are `brain_vault_nodes` rows with `node_type` in
//!   {code:function, code:struct, code:enum, code:trait, code:impl, code:mod,
//!    code:class, code:interface, code:import, code:extern}.
//!   Each symbol's `path` is the synthetic unique key
//!   `code://<corpus_slug>/<qualified_name>` and `title` holds the qualified name,
//!   so a symbol resolves by qualified name via the existing UNIQUE(path)
//!   constraint (no new column needed). `project` = corpus slug.
//!
//! EDGE MODEL (brain_vault_edges, provenance='cortex')
//!   - contains : file -> symbol, impl -> method, mod -> child
//!   - imports  : file -> code:import node holding the fully-qualified use target
//!   - calls    : caller-fn -> RESOLVED qualified callee (a real code:function
//!                when internal, else a code:extern placeholder on the same
//!                code:// path so callers_of still works)
//!
//! THE DIFFERENTIATOR — qualified call-path resolution (resolve_call):
//!   Each file derives its crate name (nearest Cargo.toml [package].name) + module
//!   segments (dir/file path under src), and builds a per-file `use` alias map
//!   handling `a::b::c`, `as`, brace groups `a::{b,c}`, and
//!   `use crate::m::{self,..}` (self binds the parent module). Leading `crate::`
//!   normalizes to the real crate name. Each call attributes to its innermost
//!   enclosing fn (byte span), and bare/self/super resolve against that caller's
//!   own module. Call shapes resolved:
//!     bare foo()          -> <caller-module>::foo
//!     self::foo()         -> <caller-module>::foo
//!     super::foo()        -> <parent-module>::foo
//!     m::foo()            -> alias-expanded if `m` is a use alias, else
//!                            <caller-module>::m::foo
//!     crate::a::b::foo()  -> <crate>::a::b::foo
//!     alias::foo()        -> <expanded-alias>::foo
//!     std/external        -> treated as already-qualified
//!   ERROR-node descent keeps functions after parse errors (else false self-edges).
//!
//! TYPESCRIPT / JAVASCRIPT (parse_typescript_file; .tsx/.jsx and plain JS use
//!   the TSX grammar so JSX parses) — module = <package.json name>::<path under
//!   pkg root> (leading `src` and a trailing `index` collapse). Imports build
//!   the same alias map (`import {a as b} from './m'`, `import * as ns`,
//!   default imports, `const x = require('./m')`), with relative sources
//!   resolved to the target file's module via the SAME path math so internal
//!   calls resolve. Calls: bare foo() (alias-first — imported fns are the
//!   dominant call form), this.m() -> the caller's class, Ns.m()/Class.m() via
//!   alias map or same-module class; unknown lower-case receivers are kept as
//!   written (code:extern, still matched by bare-leaf callers_of queries).
//!
//! JAVA (parse_java_file) — module = the file's `package` declaration
//!   (dots -> ::). Imports (`import a.b.C;`, `import static a.b.C.m;`,
//!   wildcards) feed the alias map; classes/interfaces/enums/records nest
//!   (module::Outer::Inner::method); `new Foo()` records a call to the
//!   constructor `Foo::Foo`; bare calls resolve alias-first (static imports)
//!   then to the enclosing class; Upper.m() via alias map or same-package class.
//!
//! PYTHON (parse_python_file) — module = the file's package path (ancestor dirs
//!   carrying `__init__.py`, then the file stem; `__init__.py` collapses to its
//!   package). `class` bodies nest (module::Class::method); `from m import f`
//!   binds the leaf in the alias map so bare `f()` resolves to `m::f`, while a
//!   plain `import a.b` only records the use target (the head package stays an
//!   extern receiver); `self.m()` -> the enclosing class; unknown receivers are
//!   kept as written (code:extern, still matched by bare-leaf callers_of queries).
//!
//! index() is idempotent: it DELETEs prior code:* nodes for the corpus (edges
//! cascade via brain_vault_edges ON DELETE CASCADE), then re-extracts.

use anyhow::Result;
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};
use uuid::Uuid;

#[path = "cortex/extractors/api.rs"]
pub mod api;
#[path = "cortex/audit.rs"]
pub mod audit;
#[path = "cortex/extractors/code_symbols.rs"]
pub mod code_symbols;
#[path = "cortex/extractors/config.rs"]
pub mod config;
#[path = "cortex/extractors/dataflow.rs"]
pub mod dataflow;
#[path = "cortex/extractors/db_schema.rs"]
pub mod db_schema;
#[path = "cortex/extractors/deps.rs"]
pub mod deps;
#[path = "cortex/extractors/events.rs"]
pub mod events;
#[path = "cortex/ingest_decisions.rs"]
pub mod ingest_decisions;
#[path = "cortex/ingest_fleet.rs"]
pub mod ingest_fleet;
