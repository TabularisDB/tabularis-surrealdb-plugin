# Releasing

How to cut a release and list it in the
[Tabularium registry](https://registry.tabularis.dev). Pushing a `v*` tag
runs `.github/workflows/release.yml`, which does the building and
publishing. Background on the platforms and the archive layout is in
[DESIGN.md](DESIGN.md#release-and-distribution).

## 1. Prepare `main`

1. Merge everything that goes into the release, with CI green: tests, the
   live SurrealDB v2 and v3 jobs, manifest validation, markdownlint and the
   audit.
2. Check that the versions agree. The release fails if the tag minus `v` is
   not the `.tabularium` version; keep `Cargo.toml` the same.

   ```bash
   jq -r .version .tabularium
   grep -m1 '^version' Cargo.toml
   ```

3. In `CHANGELOG.md`, replace `Unreleased` in the version's heading with
   the date, for example `## [0.1.0] - 2026-11-03`. The text under that
   heading becomes the release notes; the release fails if the section is
   missing or empty.
4. Validate the manifest against the live registry schema, as CI does:

   ```bash
   npx --yes @tabularium/cli validate .tabularium --registry https://registry.tabularis.dev --kind driver
   ```

Land these changes through a PR, for example `chore(release): 0.1.0`.

Then dry-run the release workflow on `main`. Started by hand, it builds,
packs and smoke-tests every platform (including `darwin-x64` under
Rosetta 2) and checks the glibc floor, but skips the release job and
publishes nothing:

```bash
gh workflow run release.yml -R TabularisDB/tabularis-surrealdb-plugin --ref main
gh run watch -R TabularisDB/tabularis-surrealdb-plugin
```

The archives stay attached to the run for a day, for a manual install.

## 2. Tag

```bash
git switch main
git pull --ff-only
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```

The workflow then:

1. `validate`: checks the tag against `.tabularium`.
2. `build`, one job per platform: builds the release binary, packs it with
   `.tabularium` into `surrealdb-plugin-<platform>.zip`, checks that the
   binary answers `initialize` (not on `linux-arm64`, which is
   cross-compiled), and on Linux checks that it needs glibc 2.35 or older.
3. `release`: publishes a GitHub release with the five archives and
   `.tabularium`, using the `CHANGELOG.md` section as the body. A tag with a
   `-` (`v0.2.0-beta.1`) becomes a prerelease; any other tag is marked
   latest.

If a job fails before the release is published, fix it on `main`, then move
the tag:

```bash
git push --delete origin v0.1.0
git tag -d v0.1.0
# tag and push again as above
```

Once a release has been published and picked up by the registry, don't
move its tag; release a new version instead.

## 3. Check the release

```bash
gh release view v0.1.0 -R TabularisDB/tabularis-surrealdb-plugin --json assets -q '.assets[].name'
```

Expected: `default.tabularium` (GitHub renames `.tabularium`; the registry
accepts it) and `surrealdb-plugin-` followed by `darwin-arm64`,
`darwin-x64`, `linux-arm64`, `linux-x64` and `win-x64`, each with `.zip`.
Installing one archive by hand, as described in the README, and connecting
to a server is a good last check.

## 4. Submit to the registry (first release only)

1. Sign in at <https://registry.tabularis.dev/submit> with a GitHub account
   that has admin rights on `TabularisDB/tabularis-surrealdb-plugin`, and
   pick the repository.
2. The preview reads `.tabularium` from the latest release. It should show
   version `0.1.0`, the id `surrealdb` and no validation errors. The id
   becomes the plugin's permanent slug.
3. Submit. The registry installs a release webhook on the repository. If it
   cannot, it shows a webhook URL and secret to add under the repository's
   Settings, Webhooks.
4. If the registry needs an admin's approval, the plugin stays pending until
   then.
5. In Tabularis 0.26.0 or later, open Settings, Plugins, search for
   SurrealDB and install it.

Later releases need only steps 1 to 3: the webhook adds each published
release to the registry.
