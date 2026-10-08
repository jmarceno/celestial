# Development exclusions

Reviewed on 2026-10-08 against the [TIOBE September 2026 top 20](https://www.tiobe.com/tiobe-index/), the edition available when this list was prepared. Popularity is a selection reference; exclusion decisions use recognizable tool caches and generated outputs.

Rules are editable lines in `.sync-exclude.lst`. They are created only when that file is absent. Existing lists, including an empty list or a list with removed defaults, are preserved. Both local and remote paths use the same rules.

## Top 20 coverage

| Rank | Language | Default cache/output coverage |
| --- | --- | --- |
| 1 | Python | `__pycache__`, test/type/lint caches, `.hypothesis`, `.tox`, `.nox`, notebook checkpoints and home/XDG caches |
| 2 | C | CMake/Autotools/Ninja caches and native objects/precompiled headers inside `build` or `obj` |
| 3 | C++ | Same native tool caches; compiled objects and precompiled headers inside `build` or `obj` |
| 4 | Java | Maven download cache and generated `target` subtrees; Gradle caches/classes/intermediates |
| 5 | C# | NuGet caches, `bin/Debug`, `bin/Release`, `obj/Debug`, `obj/Release` and NuGet restore metadata |
| 6 | JavaScript | npm/Yarn/pnpm caches, `node_modules`, framework outputs and bundler caches |
| 7 | Visual Basic | Same .NET build profiles and NuGet caches as C# |
| 8 | SQL | dbt's `target/partial_parse.msgpack`, SQL project restore/build metadata and shared client/runtime caches |
| 9 | R | Package-check output `*.Rcheck` and shared runtime/XDG caches |
| 10 | Rust | Cargo download caches and `builtin:rust-target` |
| 11 | Fortran | Native build caches, objects/submodule metadata in `build` or `obj`, and system scratch directories |
| 12 | Go | Entire `~/go`, anchored to the home root; shared Go build caches under XDG cache |
| 13 | Delphi/Object Pascal | `*.identcache`, plus compiled `.dcu`/`.ppu` units inside `build` or `obj` |
| 14 | PHP | Composer download cache, PHPUnit, PHP-CS-Fixer and PHP_CodeSniffer cache files |
| 15 | Scratch | Shared browser/Electron caches and system temporary directories; project archives remain included |
| 16 | Assembly | Native build caches, objects in `build` or `obj` and system temporary directories |
| 17 | Ada | Native build caches and `.ali`/object files inside `build` or `obj` |
| 18 | Swift | `.build`, SwiftPM caches and Xcode DerivedData in the conventional home location |
| 19 | Objective-C | Native build caches and conventional Xcode DerivedData |
| 20 | COBOL | Shared native build caches, object files inside `build` or `obj` and system temporary directories |

SQL and Scratch have no additional broad file-extension exclusion: SQL scripts, database files/journals and Scratch `.sb2`/`.sb3` projects are restoration inputs.

## Additional local languages

The requested `~/Projetos` directory was absent; the inventory used `/home/jmarceno/Projects`, the parent of this checkout. The read-only inventory checked source filenames and manifests across active, archived and cloned projects, omitting dependency/build/cache directories. The extra languages and tools found were:

| Language/ecosystem | Added or shared default coverage |
| --- | --- |
| TypeScript; Svelte/Vue | `*.tsbuildinfo`, `.svelte-kit`, `.nuxt`, `.next`, `.vite`, `.turbo`, `node_modules` and package-manager caches |
| Kotlin | Cache subtrees in `.gradle`/`.kotlin`, generated Gradle subtrees and Kotlin/Native caches/dependencies; tool settings remain included |
| Elixir; BEAM tools | `_build`, `.elixir_ls` and Hex package cache; source dependencies in `deps` remain included |
| D | `.dub/build` and home DUB package cache; `.d` sources and DUB settings remain included |
| Duende | `duende_build`, documented and ignored by the local compiler repository; `.du` and ordinary `.d` sources remain included |
| Haskell | `.stack-work`, `dist-newstyle`, Cabal package/store caches and Stack Pantry |
| Lua | `luac.out` and shared home/XDG caches; `.lua` sources remain included |
| GDScript/Godot | Generated `.godot` directory; scene files, scripts and persistent `.uid` files remain included |
| CUDA; QML | Existing NVIDIA caches, native build caches and Qt's home/XDG caches |
| Shell; Nix; PowerShell | Shared temporary directories/XDG caches and generated `.direnv`; scripts, `.envrc` and Nix lockfiles remain included |

## Scope of the rules

`builtin:rust-target` recognizes a real directory named `target` containing `debug`, `release` or `.rustc_info.json`, locally or in the remote listing. It excludes that entire build directory. Other tool rules identify their generated subtrees without excluding every directory named `target` or `build`.

The native object rule is `regex:(^|/)(build|obj)/.*\.(o|ali|smod|dcu|ppu|class|gch|pch)$`. It preserves source `.d` files, model `.obj` files, music `.mod` files and prebuilt libraries/executables outside the identified outputs. Tool settings, credentials, lockfiles, R session data/history, Python's manually maintained virtual environments and database WAL/SHM files remain included.

`node_modules`, downloaded dependency caches and generated directories are expected to be recreated by their toolchains. All rules can be removed before synchronization when a particular project needs those contents preserved.

## Sources used for cache choices

- [CMake cache](https://cmake.org/cmake/help/book/mastering-cmake/chapter/CMake%20Cache.html), [GNU Fortran scratch directories](https://gcc.gnu.org/onlinedocs/gfortran/TMPDIR.html)
- [Maven layout](https://maven.apache.org/guides/introduction/introduction-to-the-standard-directory-layout.html), [.NET output paths](https://learn.microsoft.com/en-us/visualstudio/ide/how-to-change-the-build-output-directory?view=vs-2022)
- [NuGet cache locations](https://learn.microsoft.com/en-us/nuget/consume-packages/managing-the-global-packages-and-cache-folders), [dbt partial parsing cache](https://docs.getdbt.com/reference/parsing)
- [Swift package manager](https://docs.swift.org/main/documentation/packagemanagerdocs/), [TypeScript build information](https://www.typescriptlang.org/tsconfig/tsBuildInfoFile.html)
- [Mix project build paths](https://mix.hexdocs.pm/Mix.Project.html), [DUB](https://dub.pm/advanced_usage/), [Stack work directories](https://docs.haskellstack.org/en/stable/topics/stack_work/)
- [Lua compiler](https://www.lua.org/manual/5.4/luac.html), [Godot version control guidance](https://docs.godotengine.org/en/stable/tutorials/best_practices/version_control_systems.html)
- [GitHub's maintained ignore templates](https://github.com/github/gitignore), consulted selectively rather than copied wholesale: many ignore version-control secrets, session state or source-like extensions that a backup must retain.
- Duende: the local `/home/jmarceno/Projects/duende/README.md` documents `examples/duende_build` output, and its `.gitignore` excludes `duende_build/`.
