# Third-Party Notices

wsx release archives include a separately executable copy of Herdr.

## Herdr

- Version: 0.8.2
- Source: https://github.com/herdrdev/herdr/tree/v0.8.2
- Commit: `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c`
- License: Apache License 2.0
- License file: `LICENSE-herdr`

Herdr remains a separate program. wsx invokes its public command-line and
protocol interfaces and does not incorporate Herdr as a library.

The archive also includes license files for Herdr's vendored
`libghostty-vt` and `portable-pty` components. Their source and patch records
are retained under `vendor/herdr/vendor/` in the wsx source repository.
