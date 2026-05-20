# Homebrew tap for ff + forgefleetd

**Status:** plan only — converts `ff`/`forgefleetd` from `direct`
install source to `brew` so they show up in `brew list`, `brew
upgrade`, and inherit Homebrew's auto-bottle download.

## What this changes

Today `ff software list` shows for every Mac:

```
taylor   ff               binary  2026.5.19_15   -   direct   ok
taylor   forgefleetd      binary  2026.5.19_14   -   direct   ok
```

After this plan lands:

```
taylor   ff               binary  2026.5.20_01   2026.5.20_01   brew   ok
taylor   forgefleetd      binary  2026.5.20_01   2026.5.20_01   brew   ok
```

i.e. tracked the same way `gh` / `openclaw` / `codex` / `op` already
are. Same `brew upgrade` flow, no more `~/.local/bin/install` step
in upgrade playbooks for Mac hosts.

## Why we'd do this

1. **Discoverability.** Anyone with `brew search forgefleet` can
   install the CLI without cloning the repo.
2. **One install method per OS.** macOS gets brew, Linux stays apt
   (separate plan if we want a PPA / .deb). Direct goes away.
3. **Signed bottles.** Homebrew signs every bottle, so the macOS
   code-signing gotcha from the project memory
   (`reference_macos_codesign`) goes away — `brew install` produces
   a signed binary by default.
4. **Auto-upgrade hook.** Our existing `auto_upgrade` tick already
   knows how to upgrade brew sources; once the formula is published
   the tick handles fleet-wide propagation without changes to
   `upgrade_playbooks/`.

## Why we wouldn't (yet)

1. **Public exposure.** A Homebrew tap on github.com/venkatyarl is
   public; the formula source plus every release would be visible.
   Fine for our binaries but worth a sanity check before publishing.
2. **Maintenance burden.** Each `cargo` version bump needs a tap
   commit (bottle SHA + version). The `brew bump-formula-pr` CLI
   automates this but it's another moving part.
3. **Linux split.** Linux machines stay on direct. Solving both
   in one PR doubles the scope.

## Steps

### 1. Create the tap repo

```bash
gh repo create venkatyarl/homebrew-forgefleet --public \
  --description "Homebrew tap for ForgeFleet CLI (ff) and daemon (forgefleetd)"
```

Standard tap layout:

```
homebrew-forgefleet/
├── README.md
└── Formula/
    ├── ff.rb
    └── forgefleetd.rb
```

### 2. Pick a versioning scheme for releases

Two options:

a. **`gh release create` per push to main.** Tag = build version
   (`2026.5.20_01`), asset = a tarball of the source tree, formula
   builds from source. Pros: works for any sha. Cons: builds take
   ~3 min on every Mac at install time.

b. **Pre-built bottles uploaded to GH releases.** CI matrix
   produces `ff-darwin-arm64.tar.gz` + `ff-darwin-x86_64.tar.gz`
   + `forgefleetd-*` for each tag, formula references the bottle
   URLs. Pros: install is a `curl` + `tar`. Cons: CI matrix to
   build + sign.

Recommended: **b**, but start with **a** while we shake out the
formula. Migrating later is a one-formula-edit change.

### 3. Author Formula/ff.rb (source-build variant for v1)

```ruby
class Ff < Formula
  desc "ForgeFleet CLI — distributed AI agent platform"
  homepage "https://github.com/venkatyarl/forge-fleet"
  url "https://github.com/venkatyarl/forge-fleet/archive/refs/tags/v2026.5.20_01.tar.gz"
  sha256 "TODO_after_first_release"
  license "MIT"
  head "https://github.com/venkatyarl/forge-fleet.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "--bin", "ff", "-p", "ff-terminal"
    bin.install "target/release/ff"
  end

  test do
    system "#{bin}/ff", "--version"
  end
end
```

`Formula/forgefleetd.rb` is identical except `--bin forgefleetd`
and the package name.

### 4. Update software_registry rows

```sql
UPDATE software_registry
SET version_source = jsonb_build_object(
  'method', 'brew',
  'formula', 'venkatyarl/forgefleet/ff'
)
WHERE id = 'ff';

UPDATE software_registry
SET version_source = jsonb_build_object(
  'method', 'brew',
  'formula', 'venkatyarl/forgefleet/forgefleetd'
)
WHERE id = 'forgefleetd';
```

The `brew` probe already exists (software_upstream.rs:377) and
queries `https://formulae.brew.sh/api/formula/<formula>.json`.

### 5. Update upgrade_playbooks

```toml
# config/upgrade_playbooks.toml — replace the direct-install steps:
[playbooks.ff]
macos = ["brew tap venkatyarl/forgefleet", "brew install ff",
         "launchctl kickstart -k gui/$UID/com.forgefleet.forgefleetd"]
linux = ["…direct install stays for now…"]

[playbooks.forgefleetd]
macos = ["brew tap venkatyarl/forgefleet", "brew install forgefleetd",
         "launchctl kickstart -k gui/$UID/com.forgefleet.forgefleetd"]
```

### 6. Bootstrap the existing Mac hosts

```bash
ff tasks add --capability ff --target taylor,ace,james \
  --cmd 'brew tap venkatyarl/forgefleet && brew install ff forgefleetd'
```

After install, the existing `ff` binary at `~/.local/bin/ff` should
be removed by the playbook so `which ff` resolves to brew's
`/opt/homebrew/bin/ff`.

### 7. Migrate `ff_git` / `forgefleetd_git`

Decision: keep them.

- `ff` (brew) tracks the released version — what's actually
  installed on the box.
- `ff_git` (still `github_release ref_kind=main`) tracks code
  identity — the most recent commit on origin/main, regardless of
  whether a release has been cut.

They answer different questions; both are useful.

## Estimated effort

| Step | Time |
|------|------|
| Create the tap repo + Formula/ff.rb | 30 min |
| Cut v2026.5.20_01 release on origin/forge-fleet | 5 min |
| Update software_registry + verify probe | 10 min |
| Update upgrade_playbooks + smoke-test on taylor | 30 min |
| Roll out to ace + james (other Macs) | 15 min |
| **Total** | **~90 min for v1** |

Plus a follow-up session if/when we want pre-built bottles
(option b above).
