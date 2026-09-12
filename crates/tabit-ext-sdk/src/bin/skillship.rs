//! The skill-shipping example (ROADMAP item 9's task-4 roster): a
//! package that declares nothing and carries only `skills/`. The
//! process exists because every package is a process — the entry ran,
//! so the user consented to code running — but its whole value is the
//! directory beside this binary, which the host links into
//! `~/.tabit/skills/<name>/` (EXTENSIONS.md's package layout).
//!
//! A real skills package installs as:
//!
//! ```text
//! ~/.tabit/extensions/my-skills/
//!   tabit.json     # entry: ["skillship-ext"]
//!   skills/
//!     my-skill/SKILL.md   # frontmatter: name + description
//! ```

fn main() {
    tabit_ext_sdk::serve(tabit_ext_sdk::Extension::new(vec![]));
}
