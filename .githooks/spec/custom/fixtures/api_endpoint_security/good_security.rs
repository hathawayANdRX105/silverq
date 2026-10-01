// FIXTURE: SHOULD SURVIVE - Complies with API endpoint security rules
// This file contains public API endpoints WITH proper security controls

use std::collections::HashMap;
use validator::{Validate, ValidationError};

// GOOD: Public endpoint WITH proper input validation
pub fn get_user_profile(user_id: &str) -> Result<String, ApiError> {
    // Validate input: alphanumeric, length bounds
    if user_id.is_empty() || user_id.len() > 64 || !user_id.chars().all(|c| c.is_alphanumeric()) {
        return Err(ApiError::InvalidInput("user_id must be alphanumeric, 1-64 chars".to_string()));
    }
    
    // Parameterized query prevents SQL injection
    let query = "SELECT * FROM users WHERE id = $1";
    execute_parameterized_query(query, &[user_id])
}

// GOOD: Public endpoint WITH output sanitization
pub fn search_users(query: &str) -> Result<String, ApiError> {
    // Validate and sanitize input
    let sanitized = sanitize_html(query);
    if sanitized.len() > 200 {
        return Err(ApiError::InvalidInput("query too long".to_string()));
    }
    
    // Use template engine with auto-escaping
    let results = format!("<div>Results for: {}</div>", sanitized);
    Ok(results)
}

// GOOD: Error handling DOES NOT expose internal details
pub fn process_payment(amount: &str) -> Result<String, ApiError> {
    let parsed = amount.parse::<f64>();
    match parsed {
        Ok(amt) if amt > 0.0 && amt <= 10000.0 => {
            process_transaction(amt)
        }
        Ok(_) => Err(ApiError::InvalidInput("amount out of range".to_string())),
        Err(_) => {
            // Log internally, return generic message
            log::error!("Payment processing failed for amount: {}", amount);
            Err(ApiError::Internal("payment processing failed".to_string()))
        }
    }
}

// GOOD: Authentication check BEFORE destructive operation
pub fn delete_user_account(auth_token: &str, user_id: &str) -> Result<bool, ApiError> {
    // Validate auth token first
    let claims = validate_jwt(auth_token)?;
    if claims.sub != user_id && !claims.is_admin {
        return Err(ApiError::Unauthorized);
    }
    
    // Parameterized query
    let query = "DELETE FROM users WHERE id = $1";
    execute_parameterized_query(query, &[user_id]);
    Ok(true)
}

// GOOD: Internal function - exempt
fn internal_health_check() -> String {
    "OK".to_string()
}

fn execute_parameterized_query(query: &str, params: &[&str]) -> Result<String, ApiError> {
    Ok(format!("Executed: {} with params: {:?}", query, params))
}

fn process_transaction(amount: f64) -> Result<String, ApiError> {
    Ok(format!("Processed ${:.2}", amount))
}

fn sanitize_html(input: &str) -> String {
    input
        .replace('&', "&")
        .replace('<', "<")
        .replace('>', ">")
        .replace('"', """)
        .replace('\'', "'")
}

fn validate_jwt(token: &str) -> Result<Claims, ApiError> {
    Ok(Claims { sub: "user123".to_string(), is_admin: false })
}

struct Claims {
    sub: String,
    is_admin: bool,
}

#[derive(Debug)]
enum ApiError {
    InvalidInput(String),
    Internal(String),
    Unauthorized,
}