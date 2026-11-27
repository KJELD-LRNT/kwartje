//! Tariff resolution (PLAN §4). Pure logic plus a small DB wrapper.
//!
//! For an hour entry, most specific first:
//! 1. task not billable → no tariff
//! 2. the task's own tariff (the version of its lineage valid on the date)
//! 3. the default tariff of the project
//! 4. the default tariff of the client
//! 5. the default tariff of the company
//! 6. none → not billable (still counts in hour totals)
//!
//! Only non-archived `hour` tariffs valid on the date take part. A € 0 tariff
//! is a tariff (billable at € 0,00); "no tariff" is `None`.

use sqlx::SqliteConnection;

use crate::web::AppResult;

/// A tariff as seen by the resolver. Dates are ISO strings (`YYYY-MM-DD`),
/// which compare correctly as strings.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Candidate {
    pub id: i64,
    pub lineage_id: i64,
    pub name: String,
    pub price_cents: i64,
    pub vat_bp: i64,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub is_default: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

impl Candidate {
    pub fn valid_on(&self, date: &str) -> bool {
        self.valid_from.as_str() <= date && self.valid_to.as_deref().is_none_or(|to| date <= to)
    }

    /// Usable for an entry with this client/project (company-level tariffs
    /// fit everything; client tariffs only that client; project tariffs only
    /// that project).
    pub fn fits(&self, client_id: Option<i64>, project_id: Option<i64>) -> bool {
        match (self.client_id, self.project_id) {
            (None, _) => true,
            (Some(c), None) => client_id == Some(c),
            (Some(c), Some(p)) => client_id == Some(c) && project_id == Some(p),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Task,
    Project,
    Client,
    Company,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Task => "tarief van de taak",
            Source::Project => "standaardtarief van het project",
            Source::Client => "standaardtarief van de klant",
            Source::Company => "standaardtarief van het bedrijf",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Context<'a> {
    pub task_billable: bool,
    /// Lineage of the task's own tariff, if it has one.
    pub task_lineage: Option<i64>,
    pub client_id: Option<i64>,
    pub project_id: Option<i64>,
    pub date: &'a str,
}

/// The tariff for an entry, with where it came from. `candidates` are the
/// company's non-archived hour tariffs.
pub fn resolve<'c>(
    ctx: &Context<'_>,
    candidates: &'c [Candidate],
) -> Option<(&'c Candidate, Source)> {
    if !ctx.task_billable {
        return None;
    }
    let valid = || candidates.iter().filter(|c| c.valid_on(ctx.date));
    if let Some(lineage) = ctx.task_lineage {
        if let Some(c) = valid().find(|c| c.lineage_id == lineage) {
            return Some((c, Source::Task));
        }
    }
    let default_at = |client: Option<i64>, project: Option<i64>| {
        valid().find(|c| c.is_default && c.client_id == client && c.project_id == project)
    };
    if let (Some(client), Some(project)) = (ctx.client_id, ctx.project_id) {
        if let Some(c) = default_at(Some(client), Some(project)) {
            return Some((c, Source::Project));
        }
    }
    if let Some(client) = ctx.client_id {
        if let Some(c) = default_at(Some(client), None) {
            return Some((c, Source::Client));
        }
    }
    default_at(None, None).map(|c| (c, Source::Company))
}

/// The version of `lineage` valid on `date`.
pub fn version_on<'c>(
    candidates: &'c [Candidate],
    lineage: i64,
    date: &str,
) -> Option<&'c Candidate> {
    candidates
        .iter()
        .find(|c| c.lineage_id == lineage && c.valid_on(date))
}

/// Do two validity periods overlap? (`None` = open-ended; bounds inclusive.)
pub fn periods_overlap(a_from: &str, a_to: Option<&str>, b_from: &str, b_to: Option<&str>) -> bool {
    let a_ends_before_b = a_to.is_some_and(|t| t < b_from);
    let b_ends_before_a = b_to.is_some_and(|t| t < a_from);
    !(a_ends_before_b || b_ends_before_a)
}

/// All non-archived hour tariffs of a company.
pub async fn hour_candidates(
    conn: &mut SqliteConnection,
    company_id: i64,
) -> AppResult<Vec<Candidate>> {
    candidates(conn, company_id, "hour").await
}

/// All non-archived tariffs of a company with this unit (`hour`, `km`, ...).
pub async fn candidates(
    conn: &mut SqliteConnection,
    company_id: i64,
    unit: &str,
) -> AppResult<Vec<Candidate>> {
    Ok(sqlx::query_as!(
        Candidate,
        r#"SELECT id AS "id!", lineage_id AS "lineage_id!", name, price_cents, vat_bp, client_id,
                  project_id, is_default AS "is_default: bool", valid_from, valid_to
           FROM tariffs
           WHERE company_id = ? AND unit = ? AND archived_at IS NULL
           ORDER BY valid_from DESC, id DESC"#,
        company_id,
        unit
    )
    .fetch_all(conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64, client: Option<i64>, project: Option<i64>, default: bool) -> Candidate {
        Candidate {
            id,
            lineage_id: id,
            name: format!("T{id}"),
            price_cents: id * 100,
            vat_bp: 2100,
            client_id: client,
            project_id: project,
            is_default: default,
            valid_from: "2026-01-01".into(),
            valid_to: None,
        }
    }

    fn ctx(client: Option<i64>, project: Option<i64>) -> Context<'static> {
        Context {
            task_billable: true,
            task_lineage: None,
            client_id: client,
            project_id: project,
            date: "2026-06-15",
        }
    }

    fn id(r: Option<(&Candidate, Source)>) -> Option<(i64, Source)> {
        r.map(|(c, s)| (c.id, s))
    }

    #[test]
    fn most_specific_level_wins() {
        let all = vec![
            t(1, None, None, true),          // company default
            t(2, Some(10), None, true),      // client 10 default
            t(3, Some(10), Some(100), true), // project 100 default
            t(4, None, None, false),         // company, not default
        ];
        assert_eq!(
            id(resolve(&ctx(None, None), &all)),
            Some((1, Source::Company))
        );
        assert_eq!(
            id(resolve(&ctx(Some(11), None), &all)),
            Some((1, Source::Company))
        );
        assert_eq!(
            id(resolve(&ctx(Some(10), None), &all)),
            Some((2, Source::Client))
        );
        assert_eq!(
            id(resolve(&ctx(Some(10), Some(101)), &all)),
            Some((2, Source::Client))
        );
        assert_eq!(
            id(resolve(&ctx(Some(10), Some(100)), &all)),
            Some((3, Source::Project))
        );
        // the task's own tariff beats everything, even a non-default one
        let mut c = ctx(Some(10), Some(100));
        c.task_lineage = Some(4);
        assert_eq!(id(resolve(&c, &all)), Some((4, Source::Task)));
    }

    #[test]
    fn non_billable_task_has_no_tariff() {
        let all = vec![t(1, None, None, true)];
        let mut c = ctx(Some(10), None);
        c.task_billable = false;
        c.task_lineage = Some(1);
        assert!(resolve(&c, &all).is_none());
    }

    #[test]
    fn no_default_means_none_and_zero_price_is_a_tariff() {
        assert!(resolve(&ctx(Some(10), None), &[t(1, None, None, false)]).is_none());
        let mut free = t(5, None, None, true);
        free.price_cents = 0;
        let r = resolve(&ctx(None, None), std::slice::from_ref(&free)).unwrap();
        assert_eq!(r.0.price_cents, 0);
    }

    #[test]
    fn validity_bounds_are_inclusive() {
        let mut a = t(1, None, None, true);
        a.valid_from = "2026-01-01".into();
        a.valid_to = Some("2026-06-30".into());
        assert!(a.valid_on("2026-01-01"));
        assert!(a.valid_on("2026-06-30"));
        assert!(!a.valid_on("2025-12-31"));
        assert!(!a.valid_on("2026-07-01"));
    }

    #[test]
    fn lineage_picks_the_version_for_the_date() {
        let mut old = t(1, None, None, true);
        old.valid_to = Some("2026-12-31".into());
        old.price_cents = 9500;
        let mut new = t(2, None, None, true);
        new.lineage_id = 1;
        new.valid_from = "2027-01-01".into();
        new.price_cents = 10_000;
        let all = vec![new, old];
        let mut c = ctx(None, None);
        c.task_lineage = Some(1);
        c.date = "2026-12-31";
        assert_eq!(resolve(&c, &all).unwrap().0.price_cents, 9500);
        c.date = "2027-01-01";
        assert_eq!(resolve(&c, &all).unwrap().0.price_cents, 10_000);
        // also through the company default
        c.task_lineage = None;
        assert_eq!(resolve(&c, &all).unwrap().0.price_cents, 10_000);
        assert_eq!(version_on(&all, 1, "2026-03-01").unwrap().id, 1);
        assert!(version_on(&all, 1, "2025-03-01").is_none());
    }

    #[test]
    fn expired_task_tariff_falls_through() {
        let mut task_t = t(7, None, None, false);
        task_t.valid_to = Some("2026-03-31".into());
        let all = vec![task_t, t(1, None, None, true)];
        let mut c = ctx(None, None);
        c.task_lineage = Some(7);
        assert_eq!(id(resolve(&c, &all)), Some((1, Source::Company)));
    }

    #[test]
    fn fits_level() {
        assert!(t(1, None, None, false).fits(None, None));
        assert!(t(1, None, None, false).fits(Some(3), Some(4)));
        assert!(t(1, Some(3), None, false).fits(Some(3), None));
        assert!(t(1, Some(3), None, false).fits(Some(3), Some(9)));
        assert!(!t(1, Some(3), None, false).fits(Some(4), None));
        assert!(!t(1, Some(3), Some(4), false).fits(Some(3), None));
    }

    #[test]
    fn overlap() {
        assert!(periods_overlap("2026-01-01", None, "2026-06-01", None));
        assert!(periods_overlap(
            "2026-01-01",
            Some("2026-06-01"),
            "2026-06-01",
            None
        ));
        assert!(!periods_overlap(
            "2026-01-01",
            Some("2026-05-31"),
            "2026-06-01",
            None
        ));
        assert!(!periods_overlap(
            "2026-06-01",
            None,
            "2026-01-01",
            Some("2026-05-31")
        ));
        assert!(periods_overlap(
            "2026-03-01",
            Some("2026-03-31"),
            "2026-01-01",
            Some("2026-12-31")
        ));
    }
}
