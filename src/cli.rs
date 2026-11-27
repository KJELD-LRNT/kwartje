//! Command-line interface: one binary, several subcommands.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::auth::users;
use crate::config::Config;
use crate::db;
use crate::security_log::{self, ClientInfo, Event};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "kwartje",
    version,
    about = "Kwartje: administratie voor uren, facturen, kilometers en documenten"
)]
pub struct Cli {
    /// Pad naar config.toml (standaard: $KWARTJE_CONFIG, ./config.toml, /etc/kwartje/config.toml)
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Migraties uitvoeren en de webserver starten
    Serve,
    /// Alleen databasemigraties uitvoeren
    Migrate,
    /// Gebruiker aanmaken (vraagt om het wachtwoord)
    CreateUser {
        #[arg(long)]
        name: String,
        #[arg(long)]
        email: String,
        /// beheerder, medewerker (standaard) of boekhouder
        #[arg(long, default_value = "medewerker")]
        role: String,
        /// Voor een boekhouder: id van een bedrijf dat hij mag zien (herhaalbaar)
        #[arg(long = "bedrijf")]
        companies: Vec<i64>,
    },
    /// Rol van een gebruiker wijzigen (beheerder, medewerker, boekhouder)
    SetRole {
        #[arg(long)]
        email: String,
        #[arg(long)]
        role: String,
        /// Voor een boekhouder: id van een bedrijf dat hij mag zien (herhaalbaar)
        #[arg(long = "bedrijf")]
        companies: Vec<i64>,
    },
    /// Alle tabellen als CSV (+ manifest.json) naar een map exporteren
    Export {
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Backup van app.db, CSV en documenten naar backup_dir/JJJJ-MM-DD; oude backups opruimen
    Backup,
    /// app.db en documenten terugzetten uit een backupmap (service moet gestopt zijn)
    Restore {
        /// Backupmap, bijv. /var/backups/kwartje/2026-11-25
        dir: PathBuf,
        /// Niet om bevestiging vragen
        #[arg(long)]
        yes: bool,
    },
    /// Hersteltest: backup terugzetten in een tijdelijke map en controleren (raakt de live gegevens niet)
    RestoreTest {
        /// Backupmap; standaard de nieuwste
        dir: Option<PathBuf>,
        /// De nieuwste backup gebruiken (standaard)
        #[arg(long)]
        latest: bool,
    },
    /// Conceptfacturen maken voor terugkerende facturen die aan de beurt zijn
    Recurring,
    /// Controleren of alle documenten aanwezig en ongewijzigd zijn (SHA-256)
    VerifyDocuments,
    /// Lege database vullen met nep-gegevens voor ontwikkeling
    Seed {
        /// Ontbrekende testgegevens toevoegen aan een niet-lege database
        #[arg(long)]
        force: bool,
    },
    /// Herstel: nieuw wachtwoord instellen en/of tweestapsverificatie resetten
    ResetUser {
        #[arg(long)]
        email: String,
        /// Om een nieuw wachtwoord vragen
        #[arg(long)]
        password: bool,
        /// Tweestapsverificatie verwijderen; wordt bij de volgende login opnieuw ingesteld
        #[arg(long)]
        totp: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct FilterArgs {
    /// Alleen dit bedrijf (id)
    #[arg(long)]
    pub company: Option<i64>,
    /// Vanaf datum (dd-mm-jjjj of jjjj-mm-dd)
    #[arg(long)]
    pub from: Option<String>,
    /// Tot en met datum
    #[arg(long)]
    pub to: Option<String>,
}

impl FilterArgs {
    fn filter(&self) -> Result<crate::export::Filter> {
        let date = |s: &Option<String>| -> Result<Option<time::Date>> {
            match s {
                None => Ok(None),
                Some(s) => crate::format::parse_date(s)
                    .map(Some)
                    .ok_or_else(|| anyhow::anyhow!("ongeldige datum: {s}")),
            }
        };
        Ok(crate::export::Filter {
            company_id: self.company,
            from: date(&self.from)?,
            to: date(&self.to)?,
        })
    }
}

/// Ask for a password twice without echo. `KWARTJE_PASSWORD` skips the
/// prompt (scripts, tests).
fn read_new_password() -> Result<String> {
    if let Ok(p) = std::env::var("KWARTJE_PASSWORD") {
        return Ok(p);
    }
    let first = rpassword::prompt_password("Wachtwoord: ")?;
    if let Some(msg) = crate::auth::password::check_strength(&first) {
        anyhow::bail!(msg);
    }
    let second = rpassword::prompt_password("Herhaal wachtwoord: ")?;
    if first != second {
        anyhow::bail!("de wachtwoorden komen niet overeen");
    }
    Ok(first)
}

async fn open_db(cfg: &Config) -> Result<sqlx::SqlitePool> {
    let pool = db::connect(&cfg.db_path()).await?;
    db::migrate_with_snapshot(&pool, &cfg.data_dir).await?;
    Ok(pool)
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fout: {e}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fout: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_sessions=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: Cli) -> Result<()> {
    let path = Config::locate(cli.config.as_deref())?;
    let cfg = Config::load(&path)?;
    match cli.command {
        Command::Serve => crate::web::serve(cfg).await,
        Command::Export { out, filter } => {
            let f = filter.filter()?;
            let pool = open_db(&cfg).await?;
            let m = crate::export::export_to_dir(&pool, &out, &f).await?;
            for t in &m.tables {
                println!("{:<20} {:>7} rijen", t.file, t.rows);
            }
            println!("Export geschreven naar {}", out.display());
            Ok(())
        }
        Command::Backup => {
            let r = crate::backup::backup(&cfg).await?;
            let rows: usize = r.manifest.csv.iter().map(|c| c.rows).sum();
            println!(
                "Backup klaar: {} (app.db {} bytes, {} rijen in CSV, {} documenten waarvan {} gelinkt)",
                r.dir.display(),
                r.manifest.app_db.bytes,
                rows,
                r.manifest.documents.files,
                r.manifest.documents.linked
            );
            for p in r.pruned {
                println!("Oude backup verwijderd: {}", p.display());
            }
            Ok(())
        }
        Command::Restore { dir, yes } => {
            let r = crate::backup::restore(&cfg, &dir, yes).await?;
            println!(
                "Hersteld vanuit backup van {}. Vorige gegevens staan in {}.",
                r.manifest.backup_date,
                r.aside.display()
            );
            Ok(())
        }
        Command::RestoreTest { dir, latest } => {
            crate::restore_test::cli(&cfg, if latest { None } else { dir }).await
        }
        Command::Recurring => {
            let pool = open_db(&cfg).await?;
            let n = crate::modules::recurring::generate_due(&pool, crate::format::today())
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            println!("{n} conceptfactuur/-facturen gemaakt.");
            Ok(())
        }
        Command::VerifyDocuments => {
            let pool = open_db(&cfg).await?;
            let (checked, problems) = crate::documents::verify_all(&pool, &cfg.data_dir).await?;
            for p in &problems {
                eprintln!("{}", p.message());
            }
            if problems.is_empty() {
                println!("{checked} documenten gecontroleerd: alles in orde.");
                Ok(())
            } else {
                anyhow::bail!(
                    "{} van {checked} documenten ontbreken of zijn gewijzigd; zet ze terug uit een backup",
                    problems.len()
                )
            }
        }
        Command::Seed { force } => {
            let pool = open_db(&cfg).await?;
            let r = crate::seed::seed(&pool, &cfg.data_dir, force).await?;
            for u in &r.users {
                println!(
                    "Testgebruiker {u} (wachtwoord: {})",
                    crate::seed::DEV_PASSWORD
                );
            }
            for c in &r.companies {
                println!("Testbedrijf {c}");
            }
            if r.documents > 0 {
                println!(
                    "{} testdocumenten (inkoopfacturen en bonnen, dit kwartaal).",
                    r.documents
                );
            }
            if r.trips > 0 {
                println!(
                    "{} testritten (vorige maand) met de testauto TS-123-K.",
                    r.trips
                );
            }
            if r.entries > 0 {
                println!(
                    "{} testregels uren (deze en vorige week), met klanten, tarieven en taken.",
                    r.entries
                );
            }
            if r.users.is_empty() && r.companies.is_empty() {
                println!("Niets toe te voegen.");
            } else {
                println!("Tweestapsverificatie wordt bij de eerste login ingesteld.");
            }
            Ok(())
        }
        Command::CreateUser {
            name,
            email,
            role,
            companies,
        } => {
            let role = parse_role(&role)?;
            let pool = open_db(&cfg).await?;
            let password = read_new_password()?;
            let id = users::create(&pool, &name, &email, &password, None).await?;
            users::set_role(&pool, id, role, None).await?;
            apply_companies(&pool, id, role, &companies).await?;
            cli_log(
                &pool,
                Event::UserCreated,
                id,
                &email,
                json!({"role": role.label()}),
            )
            .await;
            println!(
                "Gebruiker {email} aangemaakt als {} (id {id}). Tweestapsverificatie wordt bij de eerste login ingesteld.",
                role.label().to_lowercase()
            );
            Ok(())
        }
        Command::SetRole {
            email,
            role,
            companies,
        } => {
            let role = parse_role(&role)?;
            let pool = open_db(&cfg).await?;
            let user = users::find_by_email(&pool, &email)
                .await?
                .ok_or_else(|| anyhow::anyhow!("geen gebruiker met e-mailadres {email}"))?;
            users::set_role(&pool, user.id, role, None).await?;
            apply_companies(&pool, user.id, role, &companies).await?;
            cli_log(
                &pool,
                Event::RoleChanged,
                user.id,
                &email,
                json!({"role": role.label()}),
            )
            .await;
            println!("{email} is nu {}.", role.label().to_lowercase());
            Ok(())
        }
        Command::ResetUser {
            email,
            password,
            totp,
        } => {
            if !password && !totp {
                anyhow::bail!("geef --password en/of --totp op");
            }
            let pool = open_db(&cfg).await?;
            let user = users::find_by_email(&pool, &email)
                .await?
                .ok_or_else(|| anyhow::anyhow!("geen gebruiker met e-mailadres {email}"))?;
            if password {
                let pw = read_new_password()?;
                users::set_password(&pool, user.id, &pw, None).await?;
                cli_log(
                    &pool,
                    Event::PasswordReset,
                    user.id,
                    &email,
                    json!({"by": "opdrachtregel"}),
                )
                .await;
                println!("Wachtwoord van {email} gewijzigd.");
            }
            if totp {
                users::reset_totp(&pool, user.id, None).await?;
                cli_log(
                    &pool,
                    Event::MfaReset,
                    user.id,
                    &email,
                    json!({"by": "opdrachtregel"}),
                )
                .await;
                println!("Tweestapsverificatie van {email} verwijderd; wordt bij de volgende login opnieuw ingesteld.");
            }
            Ok(())
        }
        Command::Migrate => {
            let pool = db::connect(&cfg.db_path()).await?;
            if let Some(p) = db::migrate_with_snapshot(&pool, &cfg.data_dir).await? {
                println!("Kopie vóór de migratie: {}", p.display());
            }
            println!(
                "Database {} heeft schemaversie {}",
                cfg.db_path().display(),
                db::schema_version(&pool).await?
            );
            Ok(())
        }
    }
}

fn parse_role(s: &str) -> anyhow::Result<users::Role> {
    users::Role::parse(s).ok_or_else(|| {
        anyhow::anyhow!("onbekende rol {s}: kies beheerder, medewerker of boekhouder")
    })
}

/// A boekhouder needs at least one company; other roles see all of them.
async fn apply_companies(
    pool: &sqlx::SqlitePool,
    user_id: i64,
    role: users::Role,
    companies: &[i64],
) -> anyhow::Result<()> {
    if role != users::Role::Accountant {
        if !companies.is_empty() {
            println!("Let op: --bedrijf geldt alleen voor een boekhouder en is genegeerd.");
        }
        return Ok(());
    }
    if companies.is_empty() {
        let current = users::company_ids(pool, user_id).await?;
        if current.is_empty() {
            println!("Let op: deze boekhouder ziet nog geen bedrijf. Geef --bedrijf <id> op.");
        }
        return Ok(());
    }
    let all = crate::modules::companies::options(pool).await?;
    for c in companies {
        if !all.iter().any(|o| o.id == *c) {
            anyhow::bail!("onbekend of gearchiveerd bedrijf: {c}");
        }
    }
    users::set_company_ids(pool, user_id, companies, None).await?;
    Ok(())
}

async fn cli_log(
    pool: &sqlx::SqlitePool,
    e: Event,
    user_id: i64,
    email: &str,
    detail: serde_json::Value,
) {
    security_log::record(pool, e, Some(user_id), email, &ClientInfo::cli(), detail).await;
}
