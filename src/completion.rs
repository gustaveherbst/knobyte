//! `knobyte completion <bash|zsh|fish>`: shell completion for top-level commands and their
//! subcommands. The command tree is passed in (built from the CLI definition), so the scripts
//! never fall out of date.

/// (command, subcommands)
pub type CommandTree = Vec<(String, Vec<String>)>;

pub const SHELLS: &[&str] = &["bash", "zsh", "fish"];

pub fn generate(shell: &str, bin: &str, tree: &CommandTree) -> Result<String, String> {
    match shell {
        "bash" => Ok(bash(bin, tree)),
        "zsh" => Ok(zsh(bin, tree)),
        "fish" => Ok(fish(bin, tree)),
        other => Err(format!("Unknown shell \"{}\". Use bash, zsh, or fish.", other)),
    }
}

fn top(tree: &CommandTree) -> String {
    tree.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(" ")
}

fn func_name(bin: &str) -> String {
    bin.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

fn bash(bin: &str, tree: &CommandTree) -> String {
    let f = func_name(bin);
    let mut cases = String::new();
    for (cmd, subs) in tree.iter().filter(|(_, s)| !s.is_empty()) {
        cases.push_str(&format!("    {}) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;\n", cmd, subs.join(" ")));
    }
    format!(
        "_{f}_completion() {{\n  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  if [ \"$COMP_CWORD\" -eq 1 ]; then\n    COMPREPLY=($(compgen -W \"{top}\" -- \"$cur\"))\n    return\n  fi\n  case \"${{COMP_WORDS[1]}}\" in\n{cases}    *) COMPREPLY=() ;;\n  esac\n}}\ncomplete -F _{f}_completion {bin}\n",
        f = f,
        top = top(tree),
        cases = cases,
        bin = bin
    )
}

fn zsh(bin: &str, tree: &CommandTree) -> String {
    let f = func_name(bin);
    let mut cases = String::new();
    for (cmd, subs) in tree.iter().filter(|(_, s)| !s.is_empty()) {
        cases.push_str(&format!("      {}) _values 'subcommand' {} ;;\n", cmd, subs.join(" ")));
    }
    format!(
        "#compdef {bin}\n_{f}() {{\n  if (( CURRENT == 2 )); then\n    _values 'command' {top}\n  elif (( CURRENT == 3 )); then\n    case \"$words[2]\" in\n{cases}    esac\n  fi\n}}\ncompdef _{f} {bin}\n",
        bin = bin,
        f = f,
        top = top(tree),
        cases = cases
    )
}

fn fish(bin: &str, tree: &CommandTree) -> String {
    let mut out = String::new();
    let all = top(tree);
    for (cmd, subs) in tree {
        out.push_str(&format!("complete -c {} -f -n \"not __fish_seen_subcommand_from {}\" -a {}\n", bin, all, cmd));
        for s in subs {
            out.push_str(&format!("complete -c {} -f -n \"__fish_seen_subcommand_from {}\" -a {}\n", bin, cmd, s));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_list_commands() {
        let tree: CommandTree = vec![("check".into(), vec![]), ("graph".into(), vec!["status".into(), "rebuild".into()])];
        let b = generate("bash", "knobyte", &tree).unwrap();
        assert!(b.contains("complete -F _knobyte_completion knobyte") && b.contains("graph) COMPREPLY"));
        let z = generate("zsh", "knobyte", &tree).unwrap();
        assert!(z.starts_with("#compdef knobyte") && z.contains("_values 'subcommand' status rebuild"));
        let f = generate("fish", "knobyte", &tree).unwrap();
        assert!(f.contains("__fish_seen_subcommand_from graph\" -a rebuild"));
        assert!(generate("pwsh", "knobyte", &tree).is_err());
    }
}
