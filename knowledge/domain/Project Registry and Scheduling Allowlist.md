---
slug: project-registry-and-scheduling-allowlist
kind: domain
title: Project Registry and Scheduling Allowlist
description: Project identity, registry concurrency, validation, and scheduling-allowlist rules in asched.
keywords: [Project, ProjectRegistry, RegistryStore, projects.toml, canonical path, working directory, optimistic revision, scheduling allowlist, cron admission, event admission, unregister]
created: 2026-07-24
modified: 2026-08-05
---

# Project Registry and Scheduling Allowlist

## Identity and Validation

- A project is a unique canonical working-directory path plus a human name used to group and filter routines.
- Names reject empty values, control characters, path separators, and duplicates.
- Working-directory paths must resolve to existing directories.

## Concurrency and Scheduling

- Registry mutations use optimistic revisions, preventing concurrent writers from silently overwriting one another.
- Once `projects.toml` exists, it is the daemon's scheduling allowlist. Cron and project-scoped event fire both pass through it.
- Removing a project prevents future cron or event admission without deleting routine history or event receipts.
