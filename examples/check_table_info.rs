use rusqlite::Connection;

fn main() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE test (version INTEGER PRIMARY KEY, checksum TEXT NOT NULL, applied_at TEXT NOT NULL)",
        [],
    ).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(test)").unwrap();
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, bool>(5)?,
        ))
    }).unwrap();
    for row in rows {
        println!("{:?}", row.unwrap());
    }
}
