# Captain

Captain checks Cap'n Proto schema changes for backwards-incompatible edits.

It shells out to the installed `capnp` compiler first, so each side must be a valid
set of schemas. After compilation succeeds, Captain compares the old and new
schema surface and reports changes that can break old readers, old writers, or
generated RPC clients.

## Validation Modes

Captain has three ways to choose the two schema sets.

### Filesystem Paths

Use this when both schema versions are already available on disk.

```sh
captain check \
  --before tests/cases/compatible-additions/old/**/*.capnp \
  --after tests/cases/compatible-additions/new/**/*.capnp
```

This compares:

```text
before = files matched by --before
after  = files matched by --after
```

This mode is useful for generated fixtures, release artifacts, or comparing two
checked-out directories.

### Git Ref To Git Ref

Use this when both schema versions are committed.

```sh
captain check \
  --before-ref origin/main \
  --after-ref HEAD \
  --path 'schemas/**/*.capnp'
```

This compares:

```text
before = schemas/**/*.capnp from origin/main
after  = schemas/**/*.capnp from HEAD
```

Captain exports both refs into temporary directories with `git archive`, rewrites
the path glob under each export, and runs the normal comparison. The working tree
does not affect this mode.

### Git Ref To Current Worktree

Use this for the common local workflow: compare current on-disk edits against a
baseline branch.

```sh
captain check \
  --compare-ref origin/main \
  --path 'schemas/**/*.capnp'
```

This compares:

```text
before = schemas/**/*.capnp from origin/main
after  = schemas/**/*.capnp from the current working tree
```

The worktree side includes committed files, staged changes, unstaged changes, and
untracked `.capnp` files under the requested path. Captain snapshots the current
worktree into a temporary directory before checking, excluding `.git` and
`target`.

## Globs And Imports

`--before`, `--after`, and `--path` accept files, directories, and glob patterns.
Supported glob operators are `*`, `?`, and `**`.

Quote recursive globs so your shell does not rewrite them unexpectedly:

```sh
captain check --compare-ref origin/main --path 'schemas/**/*.capnp'
```

Relative import paths are supported with `-I` or `--import-path`:

```sh
captain check \
  --before-ref origin/main \
  --after-ref HEAD \
  --path 'schemas/**/*.capnp' \
  -I schemas
```

In git modes, relative import paths are resolved separately under each temporary
export or worktree snapshot.

## Exit Codes

```text
0  compatible
1  incompatible schema changes found
2  usage error, capnp compile error, git error, or other tool error
```

## Examples Of Caught Changes

Captain reports incompatibilities by schema node and ordinal.

### Field Type Changed

From `tests/cases/field-type-changed`:

```capnp
struct User {
  email @1 :Text;
}
```

changed to:

```capnp
struct User {
  primaryEmail @1 :Data;
}
```

Output:

```text
incompatible: User.field[1]
  reason: field type changed
  before: email: Text
  after: primaryEmail: Data
```

The field rename is not the problem. Reusing ordinal `1` with `Data` instead of
`Text` is the incompatible change.

### Removed Field

From `tests/cases/removed-field`:

```capnp
struct User {
  id @0 :UInt64;
  email @1 :Text;
}
```

changed to:

```capnp
struct User {
  id @0 :UInt64;
}
```

Output:

```text
incompatible: User.field[1]
  reason: field was removed
  before: email: Text
```

### Enum Ordinal Reused

From `tests/cases/enum-ordinal-reused`:

```capnp
enum Status {
  active @0;
  disabled @1;
}
```

changed to:

```capnp
enum Status {
  active @0;
  deleted @1;
}
```

Output:

```text
incompatible: Status.enum[1]
  reason: enum ordinal was reused with a different name
  before: disabled
  after: deleted
```

### Method Signature Changed

From `tests/cases/method-param-type-changed`:

```capnp
interface Users {
  get @0 (id :UInt64) -> (email :Text);
}
```

changed to:

```capnp
interface Users {
  get @0 (id :Text) -> (email :Text);
}
```

Output:

```text
incompatible: Users.method[0]
  reason: method signature changed
  before: get (id :UInt64) -> (email :Text)
  after: get (id :Text) -> (email :Text)
```

## Test Fixtures

End-to-end fixtures live in `tests/cases/<case>/old` and
`tests/cases/<case>/new`. The integration test runner executes the real `captain`
binary against every case and snapshots status, stdout, and stderr with `insta`.

To update snapshots after intentionally changing behavior:

```sh
INSTA_UPDATE=always cargo test --offline
```
