use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use clap::arg;
use clap::command;
use clap::value_parser;
use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use tree_sitter::Node;
use tree_sitter::Parser;
use tree_sitter::Tree;
use tree_sitter_bash::LANGUAGE as bash_language;

trait GetText {
    fn text<'a>(&self, source: &'a str) -> &'a str;
}

impl<'tree> GetText for Node<'tree> {
    fn text<'a>(&self, source: &'a str) -> &'a str {
        return &source[self.start_byte()..self.end_byte()];
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;

    // requires `cargo` feature, reading name, version, author, and description from `Cargo.toml`
    let matches = command!()
        .arg(arg!(<FILE>).value_parser(value_parser!(PathBuf)))
        .arg(
            arg!(-d --dir <DIR>)
                .required(false)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            arg!(-o --out <FILE>)
                .required(false)
                .value_parser(value_parser!(PathBuf)),
        )
        .get_matches();

    let source;
    let cwd;
    if let Some(path_string) = matches.get_one::<PathBuf>("FILE") {
        source = fs::read_to_string(path_string)?;
        cwd = if let Some(dir) = matches.get_one::<PathBuf>("dir") {
            dir.to_owned()
        } else {
            PathBuf::from(path_string)
                .parent()
                .expect("file path should have parent")
                .to_owned()
        };
    } else {
        source = io::read_to_string(io::stdin())?;
        cwd = if let Some(dir) = matches.get_one::<PathBuf>("dir") {
            dir.to_owned()
        } else {
            env::current_dir()?.to_owned()
        };
    };

    let out = Bundler::new(&cwd).bundle(source, &cwd)?;

    if let Some(out_path) = matches.get_one::<PathBuf>("out") {
        fs::create_dir_all(
            out_path
                .parent()
                .ok_or(eyre!("Can't save to root directory :("))?,
        )?;
        fs::write(out_path, out)?;
    } else {
        println!("{}", out);
    }

    Ok(())
}

fn parse_file(source: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&bash_language.into())?;

    let tree = parser
        .parse(&source, None)
        .ok_or(eyre!("couldn't parse file"))?;

    return Ok(tree);
}

/// Recursively visits every node in the tree rooted at `node` and calls `f` for each node.
fn visit_node<F>(node: tree_sitter::Node, source: &str, f: &mut F) -> Result<()>
where
    F: FnMut(tree_sitter::Node) -> Result<()>,
{
    f(node)?;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, source, f)?;
    }
    return Ok(());
}

struct Bundler {
    path_relative_to: PathBuf,
    shabang: Option<String>,
    visiting: Vec<PathBuf>,
    visited: HashSet<PathBuf>,
}

impl Bundler {
    fn new(relative_to: &Path) -> Self {
        Bundler {
            path_relative_to: relative_to
                .canonicalize()
                .expect("cwd can't be canonicalized!"),
            shabang: Default::default(),
            visiting: vec![],
            visited: HashSet::new(),
        }
    }

    // Must consume self since the data managed by Bundler must be reset after each bundle
    fn bundle(mut self, source: String, cwd: &Path) -> Result<String> {
        let out = (&mut self)._bundle_from_string(source, cwd)?;
        let shabang = self.shabang.ok_or(eyre!("Shabang is missing"))?;
        return Ok(format!("{}\n\n{}", shabang, out));
    }

    fn _bundle_from_path(&mut self, path: &Path) -> Result<String> {
        if self.visiting.contains(&path.to_owned()) {
            return Err(eyre!("Circular dependencies are not supported!"));
        } else {
            self.visiting.push(path.to_owned());
        }

        let source = fs::read_to_string(path)?;
        let cwd = path
            .parent()
            .ok_or(eyre!("Can't source the root directory"))?;
        let out = self._bundle_from_string(source, cwd)?;

        self.visiting.pop();
        self.visited.insert(path.to_owned());
        return Ok(out);
    }

    fn _bundle_from_string(&mut self, source: String, cwd: &Path) -> Result<String> {
        // let pf = ParsedFile::parse_from(source.clone(), &cwd)?;
        let tree = parse_file(&source)?;

        let mut found_shabang = false;
        let mut edits = vec![];

        visit_node(tree.root_node(), &source, &mut |node| {
            match node.kind() {
                "comment" => {
                    if node.text(&source).starts_with("#!") {
                        // Initial checks
                        if found_shabang {
                            return Err(eyre!("Only one shabang per file is allowed"));
                        }
                        if node.start_position().row != 0 {
                            return Err(eyre!("The shabang must be at the top of the file"));
                        }

                        let t = node.text(&source);

                        // Compare with saved shabang
                        if let Some(shabang) = self.shabang.as_ref() {
                            if shabang != t {
                                return Err(eyre!(
                                    "Shabangs across all files must match. Found {} and {}",
                                    shabang,
                                    t
                                ));
                            }
                        } else {
                            self.shabang = Some(t.to_string());
                        }
                        found_shabang = true;

                        // Remove shabang
                        edits.push(Edit {
                            start_byte: node.start_byte(),
                            end_byte: node
                                .next_sibling()
                                .map(|n| n.start_byte())
                                .unwrap_or(node.end_byte()),
                            new_content: String::new(),
                        })
                    }
                }
                "command" => {
                    let name_node = if let Some(c) = node.child(0) {
                        c
                    } else {
                        return Ok(());
                    };
                    let command_name_text = name_node.text(&source);
                    if command_name_text == "source" || command_name_text == "." {
                        let path_str = node
                            .child(1)
                            .and_then(|n| match n.kind() {
                                "word" => Some(n.text(&source).to_string()),
                                "string" => {
                                    let s = n.text(&source);
                                    Some(s[1..s.len() - 1].to_string())
                                }
                                _ => None,
                            })
                            .ok_or(eyre!("source command missing its argument"))?;

                        let path = cwd.join(&path_str).canonicalize().wrap_err_with(|| {
                            format!("failed to get full path for source: \"{}\"", path_str)
                        })?;

                        let content = if self.visited.contains(&path) {
                            String::new()
                        } else {
                            format!(
                                "# source {}\n\n{}\n\n#########",
                                path.strip_prefix(&self.path_relative_to)
                                    .wrap_err_with(|| eyre!(
                                        "trying to access script outside of current working directory: {}",
                                        path_str
                                    ))?
                                    .to_str()
                                    .expect("couldn't convert path to string"),
                                self._bundle_from_path(&path)?
                            )
                        };

                        // Write source contents
                        edits.push(Edit {
                            start_byte: node.start_byte(),
                            end_byte: node.end_byte(),
                            new_content: content,
                        });
                    }
                }
                "command_substitution" => {
                    let sib = if let Some(sib) = node
                        .next_named_sibling()
                        .or(node.parent().and_then(|p| p.next_named_sibling()))
                    {
                        sib
                    } else {
                        return Ok(());
                    };

                    if sib.kind() == "comment" && sib.text(&source) == "# build: inline" {
                        let command_raw = node.text(&source);
                        let command = &command_raw[2..command_raw.len() - 1];
                        let output = Command::new("bash").arg("-c").arg(command).output()?;

                        if !output.status.success() {
                            return Err(eyre!(
                                "\"{}\" returned with exit code {}",
                                command,
                                output.status
                            ));
                        }

                        if output.stderr.len() > 0 {
                            eprintln!(
                                "From executed command substitution's stderr: {}",
                                std::str::from_utf8(&output.stderr)?
                            );
                        }

                        let encoded_output = BASE64_STANDARD.encode(&output.stdout);

                        edits.push(Edit {
                            start_byte: node.start_byte(),
                            end_byte: node.end_byte(),
                            new_content: format!("$(echo '{}' | base64 -d)", encoded_output),
                        });
                        edits.push(Edit {
                            start_byte: sib.start_byte(),
                            end_byte: sib.end_byte(),
                            new_content: String::new(),
                        });

                        // inline_sub_nodes.push(NodeData::from_node(node, &source));
                    }
                }
                _ => {}
            }

            return Ok(());
        })?;

        if !found_shabang {
            return Err(eyre!("A shabang is required"));
        }

        return Ok(apply_edits(source, edits)?);
    }
}

struct Edit {
    start_byte: usize,
    end_byte: usize,
    new_content: String,
}

/// Apply disjoint edits simultaneously
fn apply_edits(mut source: String, mut edits: Vec<Edit>) -> Result<String> {
    edits.sort_by_key(|e| e.start_byte);
    for i in 0..edits.len() - 1 {
        if edits[i].end_byte > edits[i + 1].start_byte {
            return Err(eyre!("edits are not disjoint"));
        }
    }

    let mut edit_offset: isize = 0;
    for edit in edits {
        source.replace_range(
            (edit.start_byte as isize + edit_offset) as usize
                ..(edit.end_byte as isize + edit_offset) as usize,
            &edit.new_content,
        );
        edit_offset +=
            edit.new_content.len() as isize - (edit.end_byte as isize - edit.start_byte as isize);
    }

    return Ok(source);
}
