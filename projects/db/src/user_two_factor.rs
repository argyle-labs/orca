//! Per-user step-up preferences. Local only, like the approvals they gate.

use std::collections::BTreeSet;

use anyhow::Result;
use contract::risk::Risk;
use contract::step_up::UserTwoFactor;
use rusqlite::{Connection, OptionalExtension, params};

/// A user with no row has not enrolled.
pub fn get(conn: &Connection, user_id: &str) -> Result<UserTwoFactor> {
    let row = conn
        .query_row(
            "SELECT enrolled, step_up_from_risk, step_up_tools FROM user_two_factor
             WHERE user_id = ?1",
            params![user_id],
            |r| {
                Ok((
                    r.get::<_, bool>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((enrolled, from_risk, tools)) = row else {
        return Ok(UserTwoFactor::default());
    };
    Ok(UserTwoFactor {
        enrolled,
        step_up_from_risk: from_risk.as_deref().and_then(Risk::parse),
        step_up_tools: serde_json::from_str::<BTreeSet<String>>(&tools)?,
    })
}

pub fn put(conn: &Connection, user_id: &str, setting: &UserTwoFactor) -> Result<()> {
    conn.execute(
        "INSERT INTO user_two_factor (user_id, enrolled, step_up_from_risk, step_up_tools)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET
             enrolled          = excluded.enrolled,
             step_up_from_risk = excluded.step_up_from_risk,
             step_up_tools     = excluded.step_up_tools",
        params![
            user_id,
            setting.enrolled,
            setting.step_up_from_risk.map(Risk::as_str),
            serde_json::to_string(&setting.step_up_tools)?,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn missing_row_is_unenrolled_default() {
        let conn = test_conn();
        assert_eq!(
            get(&conn, &utils::id::new()).unwrap(),
            UserTwoFactor::default()
        );
    }

    #[test]
    fn put_get_round_trips_and_overwrites() {
        let conn = test_conn();
        let uid = utils::id::new();
        let mut s = UserTwoFactor {
            enrolled: true,
            step_up_from_risk: Some(Risk::High),
            step_up_tools: ["garage.bucket_delete".to_string()].into(),
        };
        put(&conn, &uid, &s).unwrap();
        assert_eq!(get(&conn, &uid).unwrap(), s);
        s.step_up_from_risk = None;
        s.step_up_tools.clear();
        put(&conn, &uid, &s).unwrap();
        assert_eq!(get(&conn, &uid).unwrap(), s);
    }
}
