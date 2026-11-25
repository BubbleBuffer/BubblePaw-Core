use super::*;

fn note(tx: &Transaction, text: &str) -> Result<()> {
    tx.execute("INSERT INTO notes VALUES(?1)", [text])?;
    Ok(())
}
fn step_one(tx: &Transaction) -> Result<()> {
    note(tx, "one")
}
fn step_two(tx: &Transaction) -> Result<()> {
    note(tx, "two")
}
fn step_fails(tx: &Transaction) -> Result<()> {
    note(tx, "partial")?;
    anyhow::bail!("step failed")
}

const GOOD: &[Step] = &[
    Step {
        from: 3,
        run: step_one,
    },
    Step {
        from: 4,
        run: step_two,
    },
];
const FAILING: &[Step] = &[
    Step {
        from: 3,
        run: step_one,
    },
    Step {
        from: 4,
        run: step_fails,
    },
];

fn file(version: i64) -> Result<(tempfile::TempDir, Connection)> {
    let dir = tempfile::tempdir()?;
    let conn = Connection::open(dir.path().join("s.db"))?;
    conn.execute_batch(&format!(
        "CREATE TABLE notes(t TEXT); PRAGMA user_version={version};"
    ))?;
    Ok((dir, conn))
}

struct Case {
    name: &'static str,
    start: i64,
    steps: &'static [Step],
    /// Version left in the file.
    end: i64,
    /// Notes the steps left behind, in order.
    notes: &'static [&'static str],
    /// `Err` text fragment, or the `StoreSchemaError` (found) when refused.
    failure: Failure,
}
enum Failure {
    None,
    Step,
    Refused(i64),
}

#[test]
fn upgrade_runs_the_chain_in_order_one_transaction_per_step() -> Result<()> {
    let cases = [
        Case {
            name: "current: nothing runs",
            start: 5,
            steps: GOOD,
            end: 5,
            notes: &[],
            failure: Failure::None,
        },
        Case {
            name: "two behind: both run in order",
            start: 3,
            steps: GOOD,
            end: 5,
            notes: &["one", "two"],
            failure: Failure::None,
        },
        Case {
            name: "failing step rolls back, earlier step stays",
            start: 3,
            steps: FAILING,
            end: 4,
            notes: &["one"],
            failure: Failure::Step,
        },
        Case {
            name: "older than the oldest step: refused",
            start: 2,
            steps: GOOD,
            end: 2,
            notes: &[],
            failure: Failure::Refused(2),
        },
        Case {
            name: "newer than this build: refused",
            start: 6,
            steps: GOOD,
            end: 6,
            notes: &[],
            failure: Failure::Refused(6),
        },
    ];
    for case in cases {
        let name = case.name;
        let (_dir, mut conn) = file(case.start)?;
        let result = upgrade(&mut conn, "chat", 5, case.steps);
        match case.failure {
            Failure::None => result.expect(name),
            Failure::Step => assert!(result.expect_err(name).to_string().contains("step failed")),
            Failure::Refused(found) => {
                let error = result.expect_err(name);
                let typed = error.downcast_ref::<StoreSchemaError>().expect(name);
                assert_eq!((typed.found, typed.expected), (found, 5), "{name}");
            }
        }
        assert_eq!(version(&conn)?, case.end, "{name}: version");
        let seen: Vec<String> = conn
            .prepare("SELECT t FROM notes ORDER BY rowid")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(seen, case.notes, "{name}: effects");
    }
    Ok(())
}
