# API Endpoint Security Guidelines

## Input Validation Requirements

Public API endpoints must validate all user-controlled inputs:

1. **Type Validation**: Ensure inputs are of the correct type (integer, string, enum)
2. **Length Validation**: Enforce reasonable length limits (e.g., username: 3-64 chars, query: 1-200 chars)
3. **Format Validation**: Use regex or pattern matching for formats (email, phone, UUID)
4. **Range Validation**: Numeric inputs must have min/max bounds
5. **Parameterized Queries**: Never concatenate user input into SQL/queries

## Output Sanitization Requirements

Public API endpoints must sanitize all outputs:

1. **HTML Escaping**: All HTML output must escape special characters
2. **JSON Encoding**: All JSON responses must use proper encoding
3. **Content-Type Headers**: Ensure correct Content-Type headers are set
4. **CSP Headers**: Consider implementing Content Security Policy headers

## Error Handling Requirements

Public API endpoints must handle errors safely:

1. **No Internal Details**: Error messages must not expose stack traces, SQL queries, or internal file paths
2. **Generic Messages**: Users should see user-friendly error messages
3. **Detailed Logging**: Internal error details should be logged for debugging
4. **Error Codes**: Use proper HTTP status codes (400, 401, 500, etc.)

## Authentication Requirements

Public API endpoints must verify authentication where appropriate:

1. **JWT Validation**: Validate JWT tokens for protected endpoints
2. **API Keys**: Require API keys for non-public endpoints
3. **Session Management**: Proper session handling for stateful authentication
4. **Rate Limiting**: Implement rate limiting to prevent brute force attacks

## Security Best Practices

1. **Minimum Violating Requirements**: Even security-minimal APIs should validate input types
2. **Default Deny**: Never allow operations without explicit validation
3. **Fail Secure**: Always fail closed by default
4. **Defense in Depth**: Implement multiple layers of security controls
5. **Regular Testing**: Test APIs with malformed and malicious inputs