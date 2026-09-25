//! Shell wrappers that cd into a view automatically.
//!
//! A child process cannot change its parent's working directory, so the
//! wrapper hands `magicfs` a scratch file through `MAGICFS_CD_FILE`; the
//! binary writes the view path there and the shell function does the `cd`.
//! Going through a file rather than stdout keeps the tool's normal output
//! usable and works identically in shells with no command substitution
//! niceties (tcsh).
//!
//! The wrapper also dumps the shell's aliases into `MAGICFS_ALIASES`, so a
//! command run in the view can be one of them — see [`crate::aliases`].

use anyhow::{Result, bail};

const BASH: &str = r#"
magicfs() {
  local _mfs_file _mfs_alias _mfs_rc
  _mfs_file="$(mktemp -t magicfs-cd.XXXXXX)" || return 1
  _mfs_alias="$(mktemp -t magicfs-alias.XXXXXX)" || return 1
  alias > "$_mfs_alias"
  MAGICFS_CD_FILE="$_mfs_file" MAGICFS_ALIASES="$_mfs_alias" command magicfs "$@"
  _mfs_rc=$?
  if [ -s "$_mfs_file" ]; then
    cd "$(cat "$_mfs_file")" || _mfs_rc=$?
  fi
  rm -f "$_mfs_file" "$_mfs_alias"
  return $_mfs_rc
}
"#;

const FISH: &str = r#"
function magicfs
    set -l _mfs_file (mktemp -t magicfs-cd.XXXXXX); or return 1
    env MAGICFS_CD_FILE=$_mfs_file command magicfs $argv
    set -l _mfs_rc $status
    if test -s $_mfs_file
        cd (cat $_mfs_file)
    end
    rm -f $_mfs_file
    return $_mfs_rc
end
"#;

// tcsh has no functions, so this is an alias built from `;`-separated
// statements. `\!*` forwards the arguments; the parenthesised subshell keeps
// the setenv from leaking into the interactive shell.
const TCSH: &str = r#"
alias magicfs 'set _mfs_file=`mktemp -t magicfs-cd.XXXXXX`; set _mfs_alias=`mktemp -t magicfs-alias.XXXXXX`; alias >! "$_mfs_alias"; ( setenv MAGICFS_CD_FILE "$_mfs_file" ; setenv MAGICFS_ALIASES "$_mfs_alias" ; \magicfs \!* ) ; if ( -s "$_mfs_file" ) cd "`cat $_mfs_file`" ; rm -f "$_mfs_file" "$_mfs_alias" ; unset _mfs_file _mfs_alias'
"#;

/// The wrapper source for `shell`, or for `$SHELL` when it is `None`.
pub fn script(shell: Option<&str>) -> Result<String> {
    let name = match shell {
        Some(s) => s.to_string(),
        None => detect()?,
    };
    let body = match name.as_str() {
        // zsh's syntax here is a strict subset of bash's.
        "bash" | "zsh" | "ksh" | "sh" => BASH,
        "fish" => FISH,
        "tcsh" | "csh" => TCSH,
        other => bail!("no shell-init for `{other}` (supported: bash, zsh, fish, tcsh)"),
    };
    Ok(format!(
        "# magicfs shell integration — add to your shell rc:\n\
         #   magicfs shell-init {name} > ~/.magicfs.{name} && source it\n{body}"
    ))
}

/// The user's shell, by basename of `$SHELL`.
pub fn detect() -> Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let base = shell.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        bail!("cannot tell which shell you use — pass it: magicfs shell-init bash");
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_shell_emits_a_wrapper() {
        for sh in ["bash", "zsh", "fish", "tcsh", "csh", "ksh"] {
            let s = script(Some(sh)).unwrap();
            assert!(s.contains("MAGICFS_CD_FILE"), "{sh} wrapper lost the cd hook");
            assert!(s.contains("magicfs"), "{sh} wrapper lost the command");
        }
    }

    #[test]
    fn unknown_shell_is_rejected_with_the_supported_list() {
        let err = script(Some("nushell")).unwrap_err().to_string();
        assert!(err.contains("bash"), "got: {err}");
    }

    #[test]
    fn wrappers_call_the_binary_not_themselves() {
        // Without `command`/`\`, the wrapper would recurse infinitely.
        assert!(BASH.contains("command magicfs"));
        assert!(FISH.contains("command magicfs"));
        assert!(TCSH.contains(r"\magicfs"));
    }

    #[test]
    fn bash_and_tcsh_wrappers_hand_over_their_aliases() {
        for body in [BASH, TCSH] {
            assert!(body.contains("MAGICFS_ALIASES"));
        }
    }

    #[test]
    fn wrappers_clean_up_their_scratch_file() {
        for body in [BASH, FISH, TCSH] {
            assert!(body.contains("rm -f"), "wrapper leaks a temp file");
        }
    }
}
