# Audit archive

These directories preserve dated reviews, reproductions, and captured results.
A finding describes the code at the time of that review; later fixes may already
have addressed it. See each review's scope and the corresponding fix records.

Before publication, workstation paths, account names in captured output, and
machine aliases were replaced with neutral fixtures. Markdown links use paths
relative to this repository. Standalone probes locate the checkout from their
own file location. This normalization was also applied to the initial Git
history. The original records were backed up outside the public repository.

Captured SHA-256 values describe the source files as tested before redaction.
They are historical evidence, not checksums of the normalized files or of today's
source tree. Old line numbers and probes may target an earlier implementation;
use the current tests for regression validation.
