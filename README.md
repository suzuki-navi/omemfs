# omemfs

> *Your entire personal memory, always reachable — across every device, across every year.*

**omemfs is a personal immutable filesystem for AI-era knowledge work** — a local-first sync tool built on a content-addressed object model.

---

## Why does personal data management still feel broken in 2026?

You have terabytes of data in the cloud. You have a laptop with a fraction of that. You have Git for code — but your notes, designs, logs, and AI conversations live outside Git, scattered across tools that hide structure and bury history.

The tools we have today were not designed for this:

- **Git is excellent for collaborative source code. It is awkward for continuous personal state capture.** Source control and personal continuity are different problems. Git needs a commit message to fix state, a staging area to curate changes, and a branch for every divergence. For daily knowledge work, this ceremony gets in the way.
- **Cloud sync tools hide history and structure.** Dropbox, iCloud, and their siblings give you a current view and maybe a few versions back — not a reproducible past state, not a logical tree you can reason about.
- **AI assistants need immutable, inspectable storage.** The data your AI agent produced yesterday should be as reachable as the data you produced yourself. Today, it usually isn't.

omemfs is an attempt to fix the substrate.

---

## What omemfs is

omemfs is a **local-first, content-addressed sync tool** — not a version control system.

It uses the same hash-addressed object model as Git internally, but it is designed for continuous file sync rather than commit-based version management. There are no commits, no branches, and no history chain. Instead:

- **Two sync anchors**: the *remote root* (the authoritative remote state) and the *clone root* (the last successfully synced local state). Three-way comparison — working tree vs. clone root vs. remote root — drives push and pull.
- **No staging area, no commit messages, no untracked files.** The working directory is the truth. `push` computes the current state and uploads it in one step.
- **Stubs for logically-complete × physically-partial storage.** A stub is a local reference (hash, size, mtime) whose bytes live on S3 / GCS / Azure / another filesystem. Your 10 TB of accumulated history stays logically present and physically absent until you need it.
- **Works alongside Git, not against it.** Individual software repositories keep their Git workflow. omemfs wraps the whole environment — the Git repos, the notes, the logs, all of it — in one logical tree.

### Object model

Two object types. No history chain.

```
blob    raw file content, hash-addressed
tree    directory structure (minimised JSON, entries sorted by name)
```

The *remote root* and *clone root* are both tree hashes — pointers to the top-level tree at the last known state. They are not objects; they are stored as plain-text files.

```
tree
  ├── blob          (file content)
  ├── blob
  ├── tree          (subdirectory)
  │     ├── blob
  │     └── stub    (reference: hash + size + mtime — bytes live remotely)
  └── stub
```

---

## The mental model

Three states. Four operations. No history chain.

```
                   push
working tree  ──────────►  remote root
     ▲                          │
     │  restore                 │  pull
     │                          │
     └────  expand  ──  stub  ◄─┘

clone root: the last successfully synced state, held locally
```

Push and pull compare all three states (working tree, clone root, remote root) to detect local changes, remote changes, and conflicts.

| Operation | What it does |
|---|---|
| `push` | Compute the working tree as a root object and upload to remote. |
| `pull` | Fetch the remote root; apply changes to working tree via 3-way merge. |
| `restore` | Rematerialise a past state onto the working directory. |
| `stub` / `expand` | Swap between a real file and a local reference stub. |

There is no `add`, no `stage`, no `commit`, no `sync`. The minimal surface is intentional.

---

## A concrete scenario

You worked on a client project in 2022. The code repository is gone from your laptop. The design PDFs were stubbed to S3 three years ago. Your work logs are somewhere in the archive.

With omemfs:

```
omemfs pull                            # sync the latest remote state
omemfs expand client-a/design/         # rehydrate from S3 on demand
```

Your AI agent expands only the needed files, restores the exact environment state, and answers a question using logs from three years ago — because the full logical tree was always there.

---

## A global working tree for your entire career

A knowledge worker's directory typically contains:

- Multiple client engagements (code, documents, proposals)
- Internal tools and scripts
- Personal side projects
- Daily work logs
- AI agent conversation logs
- Team information and knowledge guides

Managing these as separate repositories makes cross-project reference, restoration, and search difficult. Cramming them into a single Git repository muddies history and complicates access control.

omemfs resolves the tension:

```
~/work/                          ← managed by omemfs
├── client-a/                    ← contains its own Git repo
├── client-b/
├── internal-tools/
├── notes/
├── daily/                       ← work logs
└── ai-logs/                     ← AI agent conversations
```

Individual Git repositories keep their branch / merge / CI workflow. omemfs holds the continuity of memory *across* project boundaries. The stub system is what makes this feasible at scale — completed projects and large files are offloaded automatically; years of work history live in one directory without local-disk pressure.

---

## Relationship to AI

omemfs is not just a data dump. It is a substrate designed so that **AI agents can think with an individual's entire memory as context.**

- An AI agent (e.g. Claude Code) can expand stubs on demand and reach past logs, designs, code, and dialogue.
- What it can reach is not limited to the current project — it extends across the whole omemfs tree.
- AI is treated as an actor that reasons on top of this memory tree, not merely a data producer.

The logical completeness provided by stubs is a hard requirement for this: even when only part of the data is physically present, the full memory is logically reachable and indexable.

---

## Design principles

| Principle | What it means in practice |
|---|---|
| **Local-first** | Every operation works offline. Remote stores the durable state; local objects are a cache. |
| **Inspectable** | JSON-based, human-readable object format. Nothing is opaque. |
| **Content-addressed** | All data is hash-addressed. Deduplication and integrity checks are built in. |
| **No history chain** | No parent pointers, no commits. Sync anchors (remote root / clone root) are just tree hashes. |
| **Reproducible** | Any past working-directory state can be restored: file contents, the directory tree, symlinks, and the tracked metadata (modification time and the executable bit). Read/write permission bits, ownership, and special bits are *not* tracked — restored files take their read/write bits from your umask. |

---

## Status

omemfs is under active development and not yet ready for production use. The CLI is implemented in Rust: `clone`, `push`, `pull`, `ls`, `cat`, `restore`, `stub`, `expand`, `pack`, `stats`, `conflict`, `config export`/`add-backup`, and `log`. Local-directory, S3, Azure, and GCS backends are all supported.

See `design/` for full design documentation.

---

## CLI overview

```
omemfs clone <remote>       # clone from a remote (creates local repository)
omemfs push [<path>]        # compute root and upload to remote
omemfs pull [<path>]        # fetch remote root; apply changes via 3-way merge
omemfs ls [--dirty] [-r]    # list entries; --dirty shows uncommitted changes
omemfs cat <hash>[:<path>]  # print object content
omemfs restore <path>       # rematerialise a path from the remote
omemfs stub <path>          # replace file content with a reference stub
omemfs expand <path>        # rehydrate a stub
omemfs pack                 # consolidate remote storage
omemfs stats                # remote/local storage and I/O statistics
omemfs conflict <subcommand> # list/clean/accept-remote/accept-local/accept-base
```

No `add`. No `stage`. No `commit`. No `sync`.

---

## Who this is for

omemfs is a good fit if you:

- Feel the limits of Git for personal data (not just source code)
- Think in terms of local-first, content-addressed, or immutable storage
- Use PKM tools (Obsidian, etc.) and want something that handles history and sync properly
- Want a unified substrate for human + AI-generated data
- Work with large binary files or data that won't fit on a laptop
- Are building or thinking about a personal data infrastructure for the AI era

It is not for you if you want a polished, stable tool today. It is a substrate in progress, built around a specific set of ideas.
