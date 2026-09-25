//! Running your shell aliases.
//!
//! `magicfs -s time ll *` has to mean what `ll *` means at your prompt, but
//! aliases live inside the interactive shell and no child process can see
//! them. So the table is fetched from the shell: the `shell-init` wrapper
//! dumps it into a file named by `MAGICFS_ALIASES` on every call, which is
//! free and exactly current; without the wrapper we ask a fresh shell to read
//! your rc file and print it — which is why this is only done when a command
//! is actually being run.
//!
//! A simple alias (`ll` → `ls -lGh`, `del` → `rm -rf`) is expanded here, so
//! the rest of magicfs sees the real program: `del *` still gets real paths
//! and still asks first. Anything cleverer — a pipe, `;`, `$var`, a history
//! reference other than `\!*` — is handed to your shell along with the
//! definitions, which runs it exactly as your prompt would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which shell's syntax the table is in, and so which shell runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Csh,
    Bash,
    Zsh,
}

#[derive(Clone, Debug)]
pub struct Aliases {
    kind: Kind,
    map: BTreeMap<String, String>,
}

/// What an alias turns a command line into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expanded {
    /// A plain command line we can run ourselves.
    Words(Vec<String>),
    /// Too clever to take apart: only the shell can run it.
    Shell,
}

/// Characters that make an alias more than a list of words.
const SHELL_SYNTAX: &[char] = &['|', ';', '&', '<', '>', '`', '$', '(', ')', '{', '}', '\\', '!', '*', '?', '['];

impl Aliases {
    /// The user's aliases, if there is any way to find them.
    pub fn load() -> Option<Aliases> {
        match std::env::var_os("MAGICFS_ALIASES") {
            Some(v) if v == "off" || v == "0" || v.is_empty() => None,
            Some(file) => {
                let text = std::fs::read_to_string(&file).ok()?;
                Some(Aliases::parse(&text, None))
            }
            None => probe(),
        }
    }

    /// Read `alias` output. Each shell prints its own dialect: tcsh
    /// `name<TAB>value` with multi-word values in parentheses, bash
    /// `alias name='value'`, zsh `name=value`.
    pub fn parse(text: &str, kind: Option<Kind>) -> Aliases {
        let kind = kind.unwrap_or_else(|| guess_kind(text));
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let entry = match kind {
                Kind::Csh => line.split_once('\t').map(|(n, v)| {
                    let v = v.strip_prefix('(').and_then(|v| v.strip_suffix(')')).unwrap_or(v);
                    (n.to_string(), v.to_string())
                }),
                Kind::Bash | Kind::Zsh => line
                    .strip_prefix("alias ")
                    .unwrap_or(line)
                    .split_once('=')
                    .map(|(n, v)| (n.to_string(), unquote(v))),
            };
            if let Some((name, value)) = entry
                && !name.is_empty()
                && !name.contains(char::is_whitespace)
            {
                map.insert(name, value);
            }
        }
        Aliases { kind, map }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// Replace a leading alias the way the shell would, following aliases of
    /// aliases (`l` → `ll` → `ls -lGh`). `None` when `words` starts with no
    /// alias at all.
    pub fn expand(&self, words: &[String]) -> Option<Expanded> {
        let (first, args) = words.split_first()?;
        self.map.get(first)?;
        let mut name = first.clone();
        let mut args: Vec<String> = args.to_vec();
        // An alias may name itself (`alias ls 'ls --color'`) — that inner
        // `ls` is the program, as it is in every shell.
        let mut used = std::collections::HashSet::new();
        while used.insert(name.clone())
            && let Some(value) = self.map.get(&name)
        {
            let Some(mut tokens) = self.words_of(value) else { return Some(Expanded::Shell) };
            match tokens.iter().position(|t| t == "!*") {
                Some(at) => {
                    tokens.splice(at..=at, args);
                }
                None => tokens.extend(args),
            }
            if tokens.is_empty() {
                return Some(Expanded::Shell);
            }
            name = tokens.remove(0);
            args = tokens;
        }
        let mut out = vec![name];
        out.extend(args);
        Some(Expanded::Words(out))
    }

    /// An alias value as plain words, or `None` if it needs a shell.
    fn words_of(&self, value: &str) -> Option<Vec<String>> {
        let mut out = Vec::new();
        for raw in split_quoted(value)? {
            // tcsh's `\!*` reads back as `!*` — the one piece of history
            // syntax we can do ourselves, and only as a word of its own.
            if raw.text == "!*" && !raw.quoted && self.kind == Kind::Csh {
                out.push(raw.text);
                continue;
            }
            if !raw.quoted && raw.text.contains(SHELL_SYNTAX) {
                return None;
            }
            out.push(if raw.quoted { raw.text } else { tilde(&raw.text) });
        }
        Some(out)
    }

    /// The argv that has the shell run `line` with these aliases defined.
    pub fn command(&self, line: &str) -> Vec<String> {
        let mut script = String::new();
        if self.kind == Kind::Bash {
            // Non-interactive bash ignores aliases unless told otherwise.
            script.push_str("shopt -s expand_aliases\n");
        }
        for (name, value) in &self.map {
            if value.contains('\n') {
                continue;
            }
            let def = match self.kind {
                Kind::Csh => format!("alias {name} {}\n", self.quote(value)),
                Kind::Bash | Kind::Zsh => format!("alias {name}={}\n", self.quote(value)),
            };
            script.push_str(&def);
        }
        script.push_str(line);
        script.push('\n');
        let shell = match self.kind {
            Kind::Csh => "tcsh",
            Kind::Bash => "bash",
            Kind::Zsh => "zsh",
        };
        // `-f`: the definitions are all here; don't read the rc file again.
        let mut argv = vec![shell.to_string()];
        if self.kind != Kind::Bash {
            argv.push("-f".to_string());
        }
        argv.extend(["-c".to_string(), script]);
        argv
    }

    /// One word, quoted so this shell passes it through untouched.
    pub fn quote(&self, word: &str) -> String {
        let body = word.replace('\'', r"'\''");
        match self.kind {
            // `!` is live even inside single quotes in csh.
            Kind::Csh => format!("'{}'", body.replace('!', r"\!")),
            Kind::Bash | Kind::Zsh => format!("'{body}'"),
        }
    }

    /// A command line, each word quoted.
    pub fn line(&self, argv: &[String]) -> String {
        let mut words = argv.iter();
        // The alias name has to stay bare, or the shell won't look it up.
        let mut out = words.next().cloned().unwrap_or_default();
        for w in words {
            out.push(' ');
            out.push_str(&self.quote(w));
        }
        out
    }
}

fn guess_kind(text: &str) -> Kind {
    let first = text.lines().find(|l| !l.is_empty()).unwrap_or("");
    if first.starts_with("alias ") {
        Kind::Bash
    } else if first.split_once('\t').is_some_and(|(n, _)| !n.contains('=')) {
        Kind::Csh
    } else {
        Kind::Zsh
    }
}

/// Ask a fresh copy of the user's shell for its aliases.
///
/// tcsh needs coaxing: a `-c` shell never sets `prompt`, and rc files
/// commonly start with `if ( ! $?prompt ) exit` — exactly the part that
/// defines aliases is skipped. So the rc file is sourced by hand with a
/// prompt set.
fn probe() -> Option<Aliases> {
    let shell = std::env::var("SHELL").ok()?;
    let base = Path::new(&shell).file_name()?.to_str()?.to_string();
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let (kind, args): (Kind, Vec<String>) = match base.as_str() {
        "tcsh" | "csh" => {
            let rc = [".tcshrc", ".cshrc"].iter().map(|f| home.join(f)).find(|p| p.exists())?;
            let script = format!(
                "set prompt='> '; source '{}' >& /dev/null; alias",
                rc.display()
            );
            (Kind::Csh, vec!["-f".into(), "-c".into(), script])
        }
        "bash" => (Kind::Bash, vec!["-ic".into(), "alias".into()]),
        "zsh" => (Kind::Zsh, vec!["-ic".into(), "alias".into()]),
        _ => return None,
    };
    let out = std::process::Command::new(&shell)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(Aliases::parse(&String::from_utf8_lossy(&out.stdout), Some(kind)))
}

struct Word {
    text: String,
    quoted: bool,
}

/// Split on whitespace, honouring '...' and "..." — enough for alias values.
/// `None` for an unbalanced quote.
fn split_quoted(s: &str) -> Option<Vec<Word>> {
    let mut out = Vec::new();
    let mut cur: Option<Word> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => out.extend(cur.take()),
            '\'' | '"' => {
                let w = cur.get_or_insert(Word { text: String::new(), quoted: false });
                w.quoted = true;
                loop {
                    match chars.next()? {
                        q if q == c => break,
                        // Variables inside double quotes still expand.
                        '$' | '`' | '!' if c == '"' => return None,
                        other => w.text.push(other),
                    }
                }
            }
            other => cur.get_or_insert(Word { text: String::new(), quoted: false }).text.push(other),
        }
    }
    out.extend(cur);
    Some(out)
}

/// Undo POSIX quoting in `alias` output: `'it'\''s'` → `it's`.
fn unquote(v: &str) -> String {
    let mut out = String::new();
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    out.push(q);
                }
            }
            '\\' => out.extend(chars.next()),
            other => out.push(other),
        }
    }
    out
}

fn tilde(word: &str) -> String {
    match (word.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => word.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The shape tcsh's `alias` prints.
    const TCSH: &str = "del\trm -rf\n\
                        l\tll\n\
                        ll\t(ls -lGh)\n\
                        ls\t(ls --color)\n\
                        pk\t(pgrep !* | cut -d ' ' -f1 | xargs kill -9)\n\
                        v\t(feh !* --scale-down)\n\
                        vimrc\t(vim ~/.vimrc)\n";

    fn tcsh() -> Aliases {
        Aliases::parse(TCSH, None)
    }

    fn expand(a: &Aliases, line: &[&str]) -> Option<Expanded> {
        a.expand(&words(line))
    }

    #[test]
    fn each_shells_listing_is_understood() {
        assert_eq!(tcsh().kind, Kind::Csh);
        assert_eq!(tcsh().map["ll"], "ls -lGh");

        let bash = Aliases::parse("alias ll='ls -l'\nalias say='echo it'\\''s'\n", None);
        assert_eq!(bash.kind, Kind::Bash);
        assert_eq!(bash.map["say"], "echo it's");

        let zsh = Aliases::parse("ll='ls -l'\nx=feh\n", None);
        assert_eq!(zsh.kind, Kind::Zsh);
        assert_eq!(zsh.map["x"], "feh");
    }

    #[test]
    fn a_simple_alias_becomes_its_words_with_the_arguments_after() {
        assert_eq!(
            expand(&tcsh(), &["del", "a.png"]),
            Some(Expanded::Words(words(&["rm", "-rf", "a.png"])))
        );
        assert_eq!(expand(&tcsh(), &["feh", "a.png"]), None, "not an alias");
    }

    #[test]
    fn aliases_of_aliases_are_followed_but_a_self_reference_is_the_program() {
        assert_eq!(
            expand(&tcsh(), &["l", "x"]),
            Some(Expanded::Words(words(&["ls", "--color", "-lGh", "x"]))),
            "l → ll → ls -lGh → ls --color -lGh"
        );
    }

    #[test]
    fn bang_star_places_the_arguments() {
        assert_eq!(
            expand(&tcsh(), &["v", "a.png", "b.png"]),
            Some(Expanded::Words(words(&["feh", "a.png", "b.png", "--scale-down"])))
        );
    }

    #[test]
    fn home_is_expanded_in_plain_words() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand(&tcsh(), &["vimrc"]),
            Some(Expanded::Words(vec!["vim".into(), format!("{home}/.vimrc")]))
        );
    }

    #[test]
    fn pipes_and_variables_are_left_to_the_shell() {
        assert_eq!(expand(&tcsh(), &["pk", "x"]), Some(Expanded::Shell));
        let a = Aliases::parse("t\t(cd $tmpdir; pwd)\n", None);
        assert_eq!(expand(&a, &["t"]), Some(Expanded::Shell));
    }

    #[test]
    fn the_shell_is_handed_the_definitions_and_the_line() {
        let a = tcsh();
        let line = a.line(&words(&["pk", "it's", "a!b"]));
        assert_eq!(line, r"pk 'it'\''s' 'a\!b'");
        let argv = a.command(&line);
        assert_eq!(&argv[..3], ["tcsh", "-f", "-c"]);
        assert!(argv[3].contains("alias pk 'pgrep \\!* | cut -d '\\'' '\\'' -f1 | xargs kill -9'\n"));
        assert!(argv[3].ends_with(&format!("{line}\n")));
    }

    #[test]
    fn a_csh_alias_really_runs_in_tcsh() {
        if std::process::Command::new("tcsh").arg("-fc").arg("true").status().is_err() {
            return;
        }
        let a = Aliases::parse("shout\t(echo !* | tr a-z A-Z)\nsh2\tshout\n", None);
        let argv = a.command(&a.line(&words(&["sh2", "it's", "fine!"])));
        let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "IT'S FINE!\n");
    }
}
