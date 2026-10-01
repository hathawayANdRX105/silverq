// FIXTURE: SHOULD BE CAUGHT - Violates API endpoint security rules
// This file contains public API endpoints WITHOUT proper security controls

use std::collections::HashMap;

// BAD: Public endpoint without input validation
pub fn get_user_profile(user_id: String) -> String {
    // Direct use of user input without validation - SQL injection risk
    let query = format!("SELECT * FROM users WHERE id = '{}'", user_id);
    execute_query(&query)
}

// BAD: Public endpoint without output sanitization
pub fn search_users(query: String) -> String {
    // User input returned directly without HTML escaping - XSS risk
    let results = format!("<div>Results for: {}</div>", query);
    results
}

// BAD: Error handling exposes internal details
pub fn process_payment(amount: String) -> String {
    let parsed = amount.parse::<f64>();
    match parsed {
        Ok(amt) => process_transaction(amt),
        Err(e) => {
            // Exposing internal error details to user
            format!("Database error: {} at line {} in transaction.rs", e, 42)
        }
    }
}

// BAD: Missing authentication check
pub fn delete_user_account(user_id: String) -> bool {
    // No auth token validation before destructive operation
    let query = format!("DELETE FROM users WHERE id = '{}'", user_id);
    execute_query(&query);
    true
}

// GOOD: Internal function - should be exempt
fn internal_health_check() -> String {
    "OK".to_string()
}

fn execute_query(query: &str) -> String {
    format!("Result for: {}", query)
}

fn process_transaction(amount: f64) -> String {
    format!("Processed ${}", amount)
}