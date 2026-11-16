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

## The window

`minka-syu ui` opens it (the .desktop entry and MinkaShell's updates
pill do). It runs `minka-syu-gui` (beside the core binary, or
`$MINKA_SYU_GUI`), a view over `minka-syu serve` on its stdio; only one
window runs per session, and a second launch raises it. The QML window
in `ui/` is kept as the fallback: `MINKA_SYU_FRONTEND=quickshell`, or
whenever `minka-syu-gui` is missing (`$MINKA_SYU_DIR` points `qs -p` at
a checkout's `ui/`). `minka-syu-gui --tab updates|browse|installed|sources`
opens on a tab, and `--detail <pkg>` on a package's detail view.

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
  below the one pacman would pick ("later repo"). These start unticked
  and are applied as explicit `repo/name` targets, because a plain
  `-Syu` never selects them. If that build replaces an installed
  package, the row says so.
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
