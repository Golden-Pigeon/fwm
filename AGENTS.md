# Implementation conventions

- Prefer existing dependencies and maintained libraries for general capabilities. Do not reimplement functionality already provided by a suitable framework.
- Use `clap_complete` for completion parsing, filesystem candidates, and shell integration. Custom completion code should only supply fwm's saved server, rule, and group candidates and its configuration context.
