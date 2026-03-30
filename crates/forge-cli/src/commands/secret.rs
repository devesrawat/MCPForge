use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use forge_core::config::{DefaultSecretResolver, SecretRef, SecretResolver};
use keyring::Entry;
use secrecy::ExposeSecret;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct Secret {
    #[command(subcommand)]
    pub action: SecretAction,
}

#[derive(Debug, Subcommand)]
pub enum SecretAction {
    Set(Set),
    Ls(Ls),
    Rm(Rm),
    Check(Check),
}

#[derive(Debug, Args)]
pub struct Set {
    #[arg(help = "Keychain username / logical secret name")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct Ls;

#[derive(Debug, Args)]
pub struct Rm {
    #[arg(help = "Secret name")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct Check {
    #[arg(help = "Ref to verify, e.g. env:GH_TOKEN or keychain:mytoken")]
    pub reference: String,
}

impl Secret {
    pub fn run(&self) -> Result<()> {
        self.action.run()
    }
}

impl SecretAction {
    pub fn run(&self) -> Result<()> {
        match self {
            SecretAction::Set(cmd) => cmd.run(),
            SecretAction::Ls(cmd) => cmd.run(),
            SecretAction::Rm(cmd) => cmd.run(),
            SecretAction::Check(cmd) => cmd.run(),
        }
    }
}

impl Set {
    pub fn run(&self) -> Result<()> {
        let pw = rpassword::prompt_password("Enter secret to store (not echoed): ")
            .context("read password")?;
        let entry = Entry::new("mcp-forge", &self.name)
            .map_err(|e| anyhow::anyhow!("invalid keychain entry: {}", e))?;
        entry
            .set_password(&pw)
            .map_err(|e| anyhow::anyhow!("failed to store in keychain: {}", e))?;
        append_secret_index(&self.name)?;
        println!("Stored '{}' in keychain (service mcp-forge)", self.name);
        Ok(())
    }
}

impl Ls {
    pub fn run(&self) -> Result<()> {
        let path = secret_index_path()?;
        if !path.exists() {
            println!("(no secrets recorded yet — use `forge secret set NAME`)");
            return Ok(());
        }
        println!("NAME");
        let f = fs::File::open(&path).with_context(|| format!("read {}", path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let t = line.trim();
            if !t.is_empty() {
                println!("{}", t);
            }
        }
        Ok(())
    }
}

impl Rm {
    pub fn run(&self) -> Result<()> {
        let entry = Entry::new("mcp-forge", &self.name)
            .map_err(|e| anyhow::anyhow!("invalid keychain entry: {}", e))?;
        entry
            .delete_credential()
            .map_err(|e| anyhow::anyhow!("keychain delete: {}", e))?;
        remove_secret_index_line(&self.name)?;
        println!(
            "Removed '{}' from keychain index (credentials may still exist until deleted)",
            self.name
        );
        Ok(())
    }
}

impl Check {
    pub fn run(&self) -> Result<()> {
        let sr = parse_secret_ref(&self.reference)?;
        let resolver = DefaultSecretResolver;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("tokio runtime")?;
        let v = rt.block_on(async { resolver.resolve("check", &sr).await })?;
        println!(
            "OK: ref resolves (length {} chars)",
            v.expose_secret().len()
        );
        Ok(())
    }
}

fn secret_index_path() -> Result<PathBuf> {
    Ok(forge_core::supervisor::data_dir()?.join("secret_names.txt"))
}

fn append_secret_index(name: &str) -> Result<()> {
    let path = secret_index_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut existing = Vec::new();
    if path.exists() {
        let f = fs::File::open(&path)?;
        for line in BufReader::new(f).lines() {
            existing.push(line?);
        }
    }
    if !existing.iter().any(|l| l.trim() == name) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{}", name)?;
    }
    Ok(())
}

fn remove_secret_index_line(name: &str) -> Result<()> {
    let path = secret_index_path()?;
    if !path.exists() {
        return Ok(());
    }
    let f = fs::File::open(&path)?;
    let kept: Vec<String> = BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| l.trim() != name)
        .collect();
    fs::write(&path, kept.join("\n") + "\n")?;
    Ok(())
}

fn parse_secret_ref(text: &str) -> Result<SecretRef> {
    #[derive(serde::Deserialize)]
    struct W {
        v: SecretRef,
    }
    let t = format!("v = {:?}", text.trim());
    let w: W = toml::from_str(&t).context("parse reference")?;
    Ok(w.v)
}
