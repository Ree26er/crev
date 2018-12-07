#![allow(deprecated)]

#[macro_use]
extern crate structopt;

use failure::format_err;

use self::prelude::*;
use cargo::{
    core::{package_id::PackageId, SourceId},
    util::important_paths::find_root_manifest_for_wd,
};
use common_failures::prelude::*;
use crev_lib::ProofStore;
use crev_lib::{self, local::Local};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use structopt::StructOpt;

mod opts;
mod prelude;

use crev_lib::{TrustOrDistrust, TrustOrDistrust::*};

struct Repo {
    manifest_path: PathBuf,
    config: cargo::util::config::Config,
}

impl Repo {
    fn auto_open_cwd() -> Result<Self> {
        cargo::core::enable_nightly_features();
        let cwd = std::env::current_dir()?;
        let manifest_path = find_root_manifest_for_wd(&cwd)?;
        let mut config = cargo::util::config::Config::default()?;
        config.configure(0, None, &None, false, false, &None, &[])?;
        Ok(Repo {
            manifest_path,
            config,
        })
    }

    fn for_every_dependency_dir(
        &self,
        mut f: impl FnMut(&PackageId, &Path) -> Result<()>,
    ) -> Result<()> {
        let workspace = cargo::core::Workspace::new(&self.manifest_path, &self.config)?;
        let specs = cargo::ops::Packages::All.to_package_id_specs(&workspace)?;
        let (package_set, _resolve) = cargo::ops::resolve_ws_precisely(
            &workspace,
            None,
            &[],
            true,  // all_features
            false, // no_default_features
            &specs,
        )?;
        let source_id = SourceId::crates_io(&self.config)?;
        let map = cargo::sources::SourceConfigMap::new(&self.config)?;
        let mut source = map.load(&source_id)?;
        source.update()?;

        for pkg_id in package_set.package_ids() {
            let pkg = package_set.get(pkg_id)?;

            if !pkg.root().exists() {
                source.download(pkg_id)?;
            }

            f(&pkg_id, &pkg.root())?;
        }

        Ok(())
    }

    fn find_dependency_dir(&self, name: &str, version: Option<&str>) -> Result<PathBuf> {
        let mut dir = None;

        self.for_every_dependency_dir(|pkg_id, path| {
            if name == pkg_id.name().as_str()
                && (version.is_none() || version == Some(&pkg_id.version().to_string()))
            {
                dir = Some(path.to_owned());
            }
            Ok(())
        })?;

        Ok(dir.ok_or_else(|| format_err!("Not found"))?)
    }
}

fn cargo_ignore_list() -> HashSet<PathBuf> {
    let mut ignore_list = HashSet::new();
    ignore_list.insert(PathBuf::from(".cargo-ok"));
    ignore_list
}

fn review_crate(args: &opts::Crate, trust: TrustOrDistrust) -> Result<()> {
    let repo = Repo::auto_open_cwd()?;
    let pkg_dir = repo.find_dependency_dir(&args.name, args.version.as_deref())?;
    let local = Local::auto_open()?;
    let crev_repo = crev_lib::repo::Repo::open(&pkg_dir)?;
    let project_config = crev_repo.try_load_project_config()?;

    let digest = crev_lib::get_recursive_digest_for_dir(&pkg_dir, &cargo_ignore_list())?;
    let passphrase = crev_common::read_passphrase()?;
    let id = local.read_current_unlocked_id(&passphrase)?;

    let review = crev_data::proof::review::ProjectBuilder::default()
        .from(id.id.to_owned())
        .project(project_config.map(|c| c.project))
        .digest(digest.into_vec())
        .score(trust.to_default_score())
        .build()
        .map_err(|e| format_err!("{}", e))?;

    let review = crev_lib::util::edit_proof_content_iteractively(
        &review.into(),
        crev_data::proof::ProofType::Project,
    )?;

    let proof = review.sign_by(&id)?;

    local.insert(&proof)?;
    Ok(())
}

fn main() -> Result<()> {
    let opts = opts::Opts::from_args();
    let opts::MainCommand::Crev(command) = opts.command;
    match command {
        opts::Command::Id(id) => match id.id_command {
            opts::IdCommand::Show => crev_lib::show_id()?,
            opts::IdCommand::Gen => crev_lib::generate_id()?,
            opts::IdCommand::Switch(args) => crev_lib::switch_id(&args.id)?,
            opts::IdCommand::List => crev_lib::list_ids()?,
        },
        opts::Command::Verify(args) => {
            let local = crev_lib::Local::auto_open()?;
            let (db, trust_set) = local.load_db(&args.trust_params.clone().into())?;

            let repo = Repo::auto_open_cwd()?;
            let ignore_list = cargo_ignore_list();
            repo.for_every_dependency_dir(|_, path| {
                let digest = crev_lib::get_dir_digest(&path, &ignore_list)?;
                let result = db.verify_digest(&digest, &trust_set);
                if args.verbose {
                    println!("{:9} {} {:40}", result, digest, path.display(),);
                } else {
                    println!("{:9} {:40}", result, path.display(),);
                }

                Ok(())
            })?;
        }
        opts::Command::Review(args) => {
            review_crate(&args, TrustOrDistrust::Trust)?;
        }
        opts::Command::Flag(args) => {
            review_crate(&args, TrustOrDistrust::Distrust)?;
        }
        opts::Command::Trust(args) => {
            let local = Local::auto_open()?;
            let passphrase = crev_common::read_passphrase()?;
            local.build_trust_proof(args.pub_ids, passphrase, Trust)?;
        }
        opts::Command::Distrust(args) => {
            let local = Local::auto_open()?;
            let passphrase = crev_common::read_passphrase()?;
            local.build_trust_proof(args.pub_ids, passphrase, Distrust)?;
        }
        opts::Command::Db(cmd) => match cmd {
            opts::Db::Git(git) => {
                let local = Local::auto_open()?;
                let status = local.run_git(git.args)?;
                std::process::exit(status.code().unwrap_or(-159));
            }
            opts::Db::Fetch => {
                let local = Local::auto_open()?;
                local.fetch_updates()?;
            }
        },
        opts::Command::ListTrustedIds(args) => {
            let local = crev_lib::Local::auto_open()?;
            let (_db, trust_set) = local.load_db(&args.trust_params.into())?;
            for id in &trust_set {
                println!("{}", id);
            }
        }
    }

    Ok(())
}