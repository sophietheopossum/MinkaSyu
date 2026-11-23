# MinkaSyu

Sophie's update utility for Arch/CachyOS: a Rust core plus a window
on AntiWebUI (`gui/`, the `minka-syu-gui` binary). Checking is MinkaSyu's job (scheduled, safe — sync DBs are
refreshed into a throwaway copy, so a check can never cause a partial
upgrade); applying is paru's job, launched interactively in a terminal.
Repo and AUR updates land in one list in one pass.

Every update carries repository provenance — where the installed
package came from and where its candidate will come from — and a
change of repository is displayed explicitly rather than silently.

Not a pamac replacement: it browses, installs and removes through paru
in a terminal, with no transaction logic of its own.

How paru ended is not lost with its window. The terminal runs
`minka-syu run …`, which runs paru under script(1) (everything it
shows also goes to a log in the state dir's `runs/`), writes how it
ended to a result file, and on a failure keeps the window open until
Enter. The window shows a failure as a card above the list: the
command, its exit status or signal (and which packages did not build),
pacman's and paru's `error:` lines (with what pacman lists under them),
each makepkg `==> ERROR:` with the build lines before it, what came
after, and the log's path. A step that exits 0 but printed error lines
that still matter (a failed pacman hook or scriptlet, dkms, mkinitcpio)
gets an amber card and keeps its terminal open as well; a mirror that
pacman gave up on for the next one does not count. A no at a question,
Ctrl+C, or closing the window at a question is a cancel: no card. Each
step has its own card until it is dismissed. From the CLI,
`minka-syu apply|install|remove` wait for the terminal and print a
failure to stderr with exit status 1. Only the newest 8 steps' files
are kept, and a clean step's log is deleted.

## The window

`minka-syu ui` opens it (the .desktop entry and MinkaShell's updates
pill do). It runs `minka-syu-gui` (beside the core binary, or
`$MINKA_SYU_GUI`), a view over `minka-syu serve` on its stdio; only one
window runs per session, and a second launch raises it. The QML window
in `ui/` is kept as the fallback: `MINKA_SYU_FRONTEND=quickshell`, or
whenever `minka-syu-gui` is missing (`$MINKA_SYU_DIR` points `qs -p` at
a checkout's `ui/`). `minka-syu-gui --tab updates|browse|installed|sources`
opens on a tab, and `--detail <pkg>` on a package's detail view.

The QML fallback does not know switch rows or the no-recorded-repository
row. It shows them as later-repo rows, and while they are unticked its
command preview lists them under `--ignore` (the group as
`(no recorded repository)`). The core drops those names before paru
runs: an unticked switch or reinstall is only left out. Its detail view
does not keep an installed build either: choosing that build, or the
first repository's copy, for a later-repo row leaves the row ticked with
nothing to do.

When the AUR has a package, its detail view links to the package's AUR
page (comments, votes, maintainer) under the repository builds.

The detail view of an installed package shows its install reason as
two buttons, Dependency and Explicit, with what requires it and what it
is optional for (pacman's "Required By" and "Optional For", versions
and sonames matched as libalpm matches them). A press opens the
terminal on `sudo pacman -D --asdeps|--asexplicit -- <pkg>` (the
"reason" step, reported like the others); the buttons show what the
local DB says, read again when that step ends, and never flip before. A
failed change says so under the buttons too, as long as its card is
there. A package nothing requires gets one more line: as a dependency
it is an orphan, which `pacman -Qdt` lists; if something has it as an
optional dependency, only `pacman -Qdtt` does. From the CLI:
`minka-syu reason [--json] <pkg>` shows it, `minka-syu reason
[--dry-run] --asdeps|--asexplicit <pkg>` changes it.

Tabs: **Updates** (below), **Browse** (search repos + AUR, tick builds
to install together), **Installed** (filter, tick to remove), and
**Sources**, shown only when a `minkarepo` is found
(`$MINKA_SYU_MINKAREPO`, PATH, `~/.local/bin`): the MinkaPKG packages
against their upstreams (`minkarepo check --tsv`, cached in the state
dir as `sources.json`). Tick what to bump and "Update & publish…" opens
the terminal on `minkarepo update <picked>`; packages marked
`auto=yes` with something to do start ticked. "don't track" writes
`track=no` into the package's `upstream` file (`minkarepo track`), and
the check stops asking about it.

What the list holds beyond `pacman -Qu`, which matches packages by
name only. Where it can, the check asks libalpm itself: `pacman -Sup
-dd` on the private DB prints `-Su`'s own target set (print mode needs
no root or lock), which settles what a plain `-Syu` installs and from
where. If that fails, the check falls back to `-Qu` and pacman.conf.

- **Later-repo updates.** A strictly newer version in a repo ranked
  below the one pacman would pick ("later repo"), for a package `-Syu`
  leaves alone, since its first repository's copy is not newer than the
  installed one. These start unticked and are applied as explicit
  `repo/name` targets, because a plain `-Syu` never selects them. If
  that build replaces an installed package, the row says so. In the
  window's detail view, any build chosen for such a row is a target too,
  except the installed one: "Keep installed build" unticks the row and
  keeps no choice (choosing it used to leave the row ticked with nothing
  to do, bpf on 23/11/2026). The repository the package was installed
  from, at the installed version, counts as the installed build even
  when it rebuilt that version in place (the Installed tab reinstalls
  such a copy). On an ordinary upgrade the same button unticks the row:
  `--ignore`, for that run. A chosen build that is installed by the next
  check (the run applied it) goes the same way, its row unticked: the
  row's own build, passed over for it, never comes ticked after it.
- **Newer builds of an upgrade.** When `-Syu` upgrades a package and a
  repo ranked below has a strictly newer build still, the row stays
  pacman's upgrade, ticked, and notes the other build ("newer build in
  extra-x86-64-v4: 620.73.04-1.1"); the detail view offers it, and
  choosing it makes the row a `repo/name` target. Offering that build as
  the row, unticked, held back pacman's own upgrade as well, since an
  unticked row is `--ignore` (nvidia-settings, libxnvctrl and sqlite on
  23/11/2026, where ALHP's builds were newer than CachyOS's only by their
  rebuild suffix). If that build replaces an installed package, the row
  says so beside the note, and once the build is chosen it is the row's
  replacement, as on a later-repo row. The
  row's repository change is pacman's own, from where the package was
  installed to pacman's candidate; choosing the noted build makes it a
  change to that build's repository instead, which may add one or take
  one away (a package installed from the lower repository changes back
  to the first one unless its build is chosen). The QML fallback shows
  such a row as a plain upgrade, without the note or what the noted
  build replaces.
- **Same-version switches.** An installed package whose build is in no
  repository, while a repository carries that version as a build of its
  own: its repository dropped it (Arch's `[multilib]` lib32-libcaca,
  rebuilt in `[minka]` at the same 0.99.beta20-2), or it came from the
  AUR or a file. Nothing is newer, so `-Qu` and `-Syu` never see it.
  Builds are told apart by build date and packager: the local DB's
  against each repository copy's, from one `pacman -Sp` per round. The
  row offers the first repository in pacman.conf order with that
  version, and reads like a repo change with the same version on both
  sides ("switch to minka's build"). Like a later-repo row it starts
  unticked and is applied as a `repo/name` target, which pacman
  reinstalls from there. Unticked, it is only left out, never passed to
  `--ignore`. A package with no recorded repository whose build a
  repository has is that repository's package, not a switch.
- **No recorded repository.** libalpm records where a package came from
  (`%INSTALLED_DB%`) only for a named target or a plain upgrade, so a
  package pacman pulls in as a dependency or a replacement has none until
  its next upgrade (78 packages on 22/11/2026, 77 of them dependencies).
  Those whose build a repository carries (same version, build date and
  packager, from the switches' lookup) form one row, counted as one
  update everywhere (the window, notifications, MinkaShell's pill), with
  its names shown on demand and tickable one by one. Ticked, each is
  reinstalled from that repository (the first in pacman.conf order whose
  copy is the build) as a `repo/name` target of the same `paru -Syu`,
  which records it. Like a switch it starts unticked and is never
  passed to `--ignore`; the CLI's `apply` includes it, less any member
  named in `--ignore` (`--ignore '(no recorded repository)'` leaves out
  all of them). IgnorePkg/IgnoreGroup keep a package out of it, and so
  does a conflict: a package that something the run may install
  conflicts with (an upgrade in `-Su`'s set, or a later-repo or switch
  row's build, ticked or not) would make pacman abort the whole
  transaction as a `repo/name` target, where without one it asks to
  remove it. A replacement keeps its package out too. A switch row is
  dropped for the same conflict. Still unseen: conflicts of dependencies
  new to the run, and the package's own `%CONFLICTS%`. In the window the
  group's box shows a mix when some names are left out; leaving a name
  out only drops its reinstall, and never carries over as `--ignore`
  when that package later gets an update of its own.
- **Install reasons survive reinstalls.** A sync reinstall keeps the
  installed package's `%REASON%` (libalpm's commit_single_pkg copies it
  from the old package), with or without `-y -u`, and paru passes
  `repo/name` targets on unchanged; checked in a scratch root on
  22/11/2026 with pacman 7.1.0 (CachyOS) and paru 2.2.0. So a switch or
  a reinstall from the row above leaves dependencies dependencies, and
  no `pacman -D` stage follows. Only `--asdeps`/`--asexplicit` (every
  target's reason) or `--needed` (skips same-version targets) would
  break that, and MinkaSyu passes neither.
- **Replacements.** A package whose `replaces` names an installed
  package, such as `minka/coreutils-uutils` replacing the orphaned
  `coreutils-hybrid`. `-Qu` never reports these. Which installed
  packages each target removes follows libalpm's own walk
  (alpm_sync_sysupgrade: installed packages in name order, the repos
  they upgrade from in order, replacers before the same name). They
  start ticked, like pacman's `Replace …? [Y/n]`, and plain `-Syu`
  applies them. Unticking one passes it to `--ignore`, which skips the
  replacement. Only the replacing repo's build can be chosen for it: a
  same-named package from another repo has no `replaces` and would
  only conflict.
- **Replaced packages.** A package that a replacement removes keeps
  its own row, marked "replaced by …", for information. Unticking the
  replacement skips it; what pacman then does to the package (upgrade,
  hold, another replacer) depends on the whole run, so the row does not
  claim an outcome, and the next check shows it. A package the same run
  installs again (an Arch split whose new part depends on it) is listed
  as an ordinary upgrade instead. A replacer that is itself replaced
  stays ticked until the outer replacement is unticked, because
  ignoring it would cancel that one too.
- **Held packages.** Packages `-Syu` skips because of pacman.conf are
  listed dimmed with a "held" badge, so a pending version is not
  forgotten: IgnorePkg/IgnoreGroup (on the installed package or the
  new build), or a newer build only in a repo without
  `Usage = Upgrade`. They cannot be ticked or given a repo, and they do
  not count towards pending updates, notifications or MinkaShell's bar
  pill.

**Untracked packages** are an error, not an update: an installed package
nothing tracks. The check lists them after the updates, and the window
in a red card above the list (dismissed until the list changes); each
with its version, install reason, what requires it, and what to do.
Two kinds:

- in no repository, and the AUR has no package of that name (one AUR RPC
  info request per check, POSTed through curl, for every candidate;
  `$MINKA_SYU_AUR_RPC` overrides the URL). paru checks such "foreign"
  packages against the AUR, so an AUR package of that name is tracked;
- a repository has the name but not this version, no repository was
  recorded, and libalpm validated it with nothing (`%VALIDATION%`
  none): installed with `-U` from a file with no signature. paru never
  asks the AUR about a name a repository has (it splits packages by
  that), so even an AUR build of it is untracked (python-async-timeout
  5.0.1-2 on 22/11/2026, the AUR's, while `[extra]` has 5.0.1-1). A
  missing `%INSTALLED_DB%` alone means nothing (dependencies never get
  one), and a signed or checksummed build is a repository's, which
  pacman tracks by name: a v4 repository's 1.68-1.1 that it dropped,
  while `[extra]` has 1.68-1, is no error.

When the AUR cannot be asked, no package of the first kind is flagged
(most are AUR packages) and an amber note says so instead. A package
whose recorded repository moved past it is tracked by that repository,
and a row (an update, a replacement) keeps a package off the list.
