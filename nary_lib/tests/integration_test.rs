use nary_lib::deps::*;
use nary_lib::{
    calculate_depends_with_config, calculate_depends_with_options, create_client, MaturityConfig,
    NpmrcConfig, RegistryConfig, ResolveOptions,
};

use indoc::indoc;
use std::io::Cursor;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use nary_lib::Result;

#[test]
fn it_will_get_dependency_version() {
    let package_json = indoc! {r###"
        {
            "private": true,
            "name": "or",
            "version": "1.0.0",
            "description": "",
            "main": "index.js",
            "scripts": {
                "test": "echo \"Error: no test specified\" && exit 1"
            },
            "author": "",
            "license": "ISC",
            "dependencies": {
                "koa-ejs": "^4.1.0"
            }
        }
    "###};

    let cursor = Cursor::new(package_json);
    let dependencies = json_to_dependencies(cursor, false, "test");

    let dependencies = dependencies.unwrap();
    let dep = dependencies.get(0).unwrap();

    assert_eq!(dep.requested, "^4.1.0");
}

#[test]
fn it_will_gather_dependencies() -> Result<()> {
    let koa_ejs = include_str!("repository/koa-ejs.json");
    let cursor = Cursor::new(koa_ejs);
    let dependencies = json_to_dependencies(cursor, false, "test");

    let dependencies = dependencies?;
    assert_eq!(dependencies.get(0).unwrap().name, "debug");
    assert_eq!(dependencies.get(1).unwrap().name, "ejs");
    assert_eq!(dependencies.get(2).unwrap().name, "mz");

    Ok(())
}

/// Mount a mock response for a package (both root and version endpoints)
async fn mount_package(server: &MockServer, name: &str, body: &str) {
    // Mount root metadata endpoint
    Mock::given(method("GET"))
        .and(path(format!("/{}", name)))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
        .mount(server)
        .await;

    // Parse the fixture to mount version-specific endpoints
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(versions) = json.get("versions").and_then(|v| v.as_object()) {
            for (version, version_data) in versions {
                let version_body = version_data.to_string();
                Mock::given(method("GET"))
                    .and(path(format!("/{}/{}", name, version)))
                    .respond_with(ResponseTemplate::new(200).set_body_string(version_body))
                    .mount(server)
                    .await;
            }
        }
    }
}

#[tokio::test]
async fn it_will_build_dependency_map_with_mock() -> Result<()> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    // Start mock server
    let mock_server = MockServer::start().await;

    // Mount all package fixtures
    mount_package(
        &mock_server,
        "debug",
        include_str!("fixtures/registry/debug.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "ms",
        include_str!("fixtures/registry/ms.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "ejs",
        include_str!("fixtures/registry/ejs.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "mz",
        include_str!("fixtures/registry/mz.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "any-promise",
        include_str!("fixtures/registry/any-promise.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "object-assign",
        include_str!("fixtures/registry/object-assign.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "thenify-all",
        include_str!("fixtures/registry/thenify-all.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "thenify",
        include_str!("fixtures/registry/thenify.json"),
    )
    .await;

    // Parse dependencies from package.json
    let koa_ejs = Cursor::new(include_str!("repository/koa-ejs.json"));
    let dependencies = json_to_dependencies(koa_ejs, false, "test")?;

    assert_eq!(dependencies.get(0).unwrap().name, "debug");
    assert_eq!(dependencies.get(1).unwrap().name, "ejs");
    assert_eq!(dependencies.get(2).unwrap().name, "mz");

    let root = Dependency {
        name: "koa_ejs".to_string(),
        requested: "1".to_string(),
        resolved: "1".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    // Configure to use mock server
    let config = RegistryConfig::with_registry(mock_server.uri());
    let client = create_client()?;

    // Calculate dependencies using mock server
    let calculated =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await?;

    // Verify we got the expected packages
    let names: Vec<&str> = calculated.keys().map(|d| d.name.as_str()).collect();

    assert!(names.contains(&"debug"));
    assert!(names.contains(&"ejs"));
    assert!(names.contains(&"mz"));
    assert!(names.contains(&"ms")); // transitive dep of debug
    assert!(names.contains(&"any-promise")); // transitive dep of mz
    assert!(names.contains(&"object-assign")); // transitive dep of mz
    assert!(names.contains(&"thenify-all")); // transitive dep of mz
    assert!(names.contains(&"thenify")); // transitive dep of thenify-all

    // Verify versions are correctly resolved
    let debug = calculated.keys().find(|d| d.name == "debug").unwrap();
    assert_eq!(debug.resolved, "2.6.9");

    let ejs = calculated.keys().find(|d| d.name == "ejs").unwrap();
    assert_eq!(ejs.resolved, "2.7.4");

    let mz = calculated.keys().find(|d| d.name == "mz").unwrap();
    assert_eq!(mz.resolved, "2.7.0");

    Ok(())
}

#[tokio::test]
async fn it_resolves_nested_dependencies() -> Result<()> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // Mount packages that form a dependency chain: parent -> child -> grandchild
    mount_package(
        &mock_server,
        "debug",
        include_str!("fixtures/registry/debug.json"),
    )
    .await;
    mount_package(
        &mock_server,
        "ms",
        include_str!("fixtures/registry/ms.json"),
    )
    .await;

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "debug".to_string(),
        requested: "^2.6.1".to_string(),
        resolved: String::new(), // Will be resolved
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let config = RegistryConfig::with_registry(mock_server.uri());
    let client = create_client()?;

    let calculated =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await?;

    // Should have both debug and its transitive dependency ms
    let names: Vec<&str> = calculated.keys().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"debug"));
    assert!(names.contains(&"ms"));

    // Verify ms was correctly resolved
    let ms = calculated.keys().find(|d| d.name == "ms").unwrap();
    assert_eq!(ms.resolved, "2.0.0");

    Ok(())
}

#[tokio::test]
async fn it_routes_scoped_packages_to_private_registry() -> Result<()> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    // Start two mock servers: default (public) and private
    let public_server = MockServer::start().await;
    let private_server = MockServer::start().await;

    // Mount debug on public registry
    mount_package(
        &public_server,
        "debug",
        include_str!("fixtures/registry/debug.json"),
    )
    .await;
    mount_package(
        &public_server,
        "ms",
        include_str!("fixtures/registry/ms.json"),
    )
    .await;

    // Mount scoped package on private registry (both root and version endpoints)
    let scoped_fixture = include_str!("fixtures/registry/@myorg/utils.json");
    Mock::given(method("GET"))
        .and(path("/@myorg/utils"))
        .respond_with(ResponseTemplate::new(200).set_body_string(scoped_fixture))
        .mount(&private_server)
        .await;

    // Mount version endpoint for scoped package
    Mock::given(method("GET"))
        .and(path("/@myorg/utils/1.0.0"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"name":"@myorg/utils","version":"1.0.0","dist":{"tarball":"https://npm.myorg.com/@myorg/utils/-/utils-1.0.0.tgz","integrity":"sha512-abc123def456=="}}"#
        ))
        .mount(&private_server)
        .await;

    // Configure scoped registry
    let mut npmrc = NpmrcConfig::default();
    npmrc
        .scoped_registries
        .insert("@myorg".to_string(), private_server.uri());

    let config = RegistryConfig::with_config(npmrc, public_server.uri());

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![
        Dependency {
            name: "debug".to_string(),
            requested: "^2.6.0".to_string(),
            resolved: String::new(),
            is_optional: false,
            alias: None,
            install_path: None,
        },
        Dependency {
            name: "@myorg/utils".to_string(),
            requested: "^1.0.0".to_string(),
            resolved: String::new(),
            is_optional: false,
            alias: None,
            install_path: None,
        },
    ];

    let client = create_client()?;
    let calculated =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await?;

    // Verify both packages were resolved
    let names: Vec<&str> = calculated.keys().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"debug"));
    assert!(names.contains(&"@myorg/utils"));
    assert!(names.contains(&"ms")); // transitive dep

    // Verify versions
    let scoped = calculated
        .keys()
        .find(|d| d.name == "@myorg/utils")
        .unwrap();
    assert_eq!(scoped.resolved, "1.0.0");

    Ok(())
}

#[tokio::test]
async fn it_sends_bearer_token_auth_header() -> Result<()> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // Extract host from mock server URI
    let uri = mock_server.uri();
    let host = uri.strip_prefix("http://").unwrap_or(&uri);

    // Mount package that requires auth - verify header is present
    Mock::given(method("GET"))
        .and(path("/debug"))
        .and(header("authorization", "Bearer secret-token-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/registry/debug.json")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ms"))
        .and(header("authorization", "Bearer secret-token-123"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/registry/ms.json")),
        )
        .mount(&mock_server)
        .await;

    // Mount version endpoints with auth
    Mock::given(method("GET"))
        .and(path("/debug/2.6.9"))
        .and(header("authorization", "Bearer secret-token-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"name":"debug","version":"2.6.9","dependencies":{"ms":"2.0.0"},"dist":{"tarball":"https://registry.npmjs.org/debug/-/debug-2.6.9.tgz","integrity":"sha512-bC7ElrdJaJnPbAP+1EotYvqZsb3ecl5wi6Bfi6BJTUcNowp6cvspg0jXznRTKDjm/E7AdgFBVeAPVMNcKGsHMA=="}}"#,
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ms/2.0.0"))
        .and(header("authorization", "Bearer secret-token-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"name":"ms","version":"2.0.0","dist":{"tarball":"https://registry.npmjs.org/ms/-/ms-2.0.0.tgz","integrity":"sha512-Tpp60P6IUJDTuOq/5Z8cdskzJujfwqfOTkrwIwj7IRISpnkJnT6SyJ4PCPnGMoFjC9ddhal5KVIYtAt97ix05A=="}}"#,
        ))
        .mount(&mock_server)
        .await;

    // Configure auth token
    let mut npmrc = NpmrcConfig::default();
    npmrc
        .auth_tokens
        .insert(host.to_string(), "secret-token-123".to_string());

    let config = RegistryConfig::with_config(npmrc, mock_server.uri());

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "debug".to_string(),
        requested: "^2.6.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let client = create_client()?;
    let calculated =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await?;

    // If we got here without error, the auth header was accepted
    let names: Vec<&str> = calculated.keys().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"debug"));

    Ok(())
}

#[tokio::test]
async fn it_sends_basic_auth_header() -> Result<()> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    let uri = mock_server.uri();
    let host = uri.strip_prefix("http://").unwrap_or(&uri);

    // Mount package that requires basic auth
    Mock::given(method("GET"))
        .and(path("/debug"))
        .and(header("authorization", "Basic dXNlcjpwYXNz")) // base64("user:pass")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/registry/debug.json")),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ms"))
        .and(header("authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/registry/ms.json")),
        )
        .mount(&mock_server)
        .await;

    // Mount version endpoints with auth
    Mock::given(method("GET"))
        .and(path("/debug/2.6.9"))
        .and(header("authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"name":"debug","version":"2.6.9","dependencies":{"ms":"2.0.0"},"dist":{"tarball":"https://registry.npmjs.org/debug/-/debug-2.6.9.tgz","integrity":"sha512-bC7ElrdJaJnPbAP+1EotYvqZsb3ecl5wi6Bfi6BJTUcNowp6cvspg0jXznRTKDjm/E7AdgFBVeAPVMNcKGsHMA=="}}"#,
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ms/2.0.0"))
        .and(header("authorization", "Basic dXNlcjpwYXNz"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"name":"ms","version":"2.0.0","dist":{"tarball":"https://registry.npmjs.org/ms/-/ms-2.0.0.tgz","integrity":"sha512-Tpp60P6IUJDTuOq/5Z8cdskzJujfwqfOTkrwIwj7IRISpnkJnT6SyJ4PCPnGMoFjC9ddhal5KVIYtAt97ix05A=="}}"#,
        ))
        .mount(&mock_server)
        .await;

    // Configure legacy auth (base64 encoded user:pass)
    let mut npmrc = NpmrcConfig::default();
    npmrc
        .legacy_auth
        .insert(host.to_string(), "dXNlcjpwYXNz".to_string());

    let config = RegistryConfig::with_config(npmrc, mock_server.uri());

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "debug".to_string(),
        requested: "^2.6.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let client = create_client()?;
    let calculated =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await?;

    let names: Vec<&str> = calculated.keys().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"debug"));

    Ok(())
}

#[tokio::test]
async fn it_handles_401_unauthorized() -> Result<(), Box<dyn std::error::Error>> {
    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // Mount package that returns 401
    Mock::given(method("GET"))
        .and(path("/private-pkg"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&mock_server)
        .await;

    let config = RegistryConfig::with_registry(mock_server.uri());

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "private-pkg".to_string(),
        requested: "^1.0.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let client = create_client()?;
    let result =
        calculate_depends_with_config(&client, &root, &dependencies, |_, _| {}, &config).await;

    // Should return an error for 401
    assert!(result.is_err());
    Ok(())
}

/// Mount a mock response with time field for maturity testing
async fn mount_package_with_time(server: &MockServer, name: &str, body: &str) {
    // Mount root metadata endpoint
    Mock::given(method("GET"))
        .and(path(format!("/{}", name)))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
        .mount(server)
        .await;

    // Parse the fixture to mount version-specific endpoints
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(versions) = json.get("versions").and_then(|v| v.as_object()) {
            for (version, version_data) in versions {
                let version_body = version_data.to_string();
                Mock::given(method("GET"))
                    .and(path(format!("/{}/{}", name, version)))
                    .respond_with(ResponseTemplate::new(200).set_body_string(version_body))
                    .mount(server)
                    .await;
            }
        }
    }
}

#[tokio::test]
async fn it_applies_maturity_fallback_to_older_version() -> Result<()> {
    use chrono::{Duration, Utc};

    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // Create mock package with time data:
    // - 1.0.1 published 1 hour ago (too new)
    // - 1.0.0 published 1 week ago (mature)
    let recent = Utc::now() - Duration::hours(1);
    let old = Utc::now() - Duration::days(7);

    let test_pkg = format!(
        r#"{{
            "name": "test-pkg",
            "versions": {{
                "1.0.0": {{
                    "name": "test-pkg",
                    "version": "1.0.0",
                    "dist": {{
                        "tarball": "https://registry.npmjs.org/test-pkg/-/test-pkg-1.0.0.tgz",
                        "integrity": "sha512-old"
                    }}
                }},
                "1.0.1": {{
                    "name": "test-pkg",
                    "version": "1.0.1",
                    "dist": {{
                        "tarball": "https://registry.npmjs.org/test-pkg/-/test-pkg-1.0.1.tgz",
                        "integrity": "sha512-new"
                    }}
                }}
            }},
            "time": {{
                "1.0.0": "{}",
                "1.0.1": "{}"
            }}
        }}"#,
        old.to_rfc3339(),
        recent.to_rfc3339()
    );

    mount_package_with_time(&mock_server, "test-pkg", &test_pkg).await;

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "test-pkg".to_string(),
        requested: "^1.0.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let config = RegistryConfig::with_registry(mock_server.uri());
    let client = create_client()?;

    // With maturity enabled (3 day requirement)
    let maturity_config = MaturityConfig {
        minimum_age_minutes: 4320, // 3 days
        excluded_packages: vec![],
        allow_new_packages: false,
    };
    let options = ResolveOptions {
        optimize: false,
        maturity: maturity_config,
        offline: false,
    };

    let calculated =
        calculate_depends_with_options(&client, &root, &dependencies, |_, _| {}, &config, &options)
            .await?;

    // Should fall back to 1.0.0 since 1.0.1 is too new
    let pkg = calculated
        .keys()
        .find(|d| d.name == "test-pkg")
        .expect("test-pkg should be resolved");
    assert_eq!(pkg.resolved, "1.0.0");

    // Should have fallback info
    let info = calculated.get(pkg).unwrap();
    assert!(info.maturity_fallback.is_some());
    let fallback = info.maturity_fallback.as_ref().unwrap();
    assert_eq!(fallback.skipped_version, "1.0.1");

    Ok(())
}

#[tokio::test]
async fn it_allows_new_packages_when_flag_set() -> Result<()> {
    use chrono::{Duration, Utc};

    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // Both versions published recently
    let recent = Utc::now() - Duration::hours(1);

    let test_pkg = format!(
        r#"{{
            "name": "new-pkg",
            "versions": {{
                "1.0.0": {{
                    "name": "new-pkg",
                    "version": "1.0.0",
                    "dist": {{
                        "tarball": "https://registry.npmjs.org/new-pkg/-/new-pkg-1.0.0.tgz",
                        "integrity": "sha512-abc"
                    }}
                }},
                "1.0.1": {{
                    "name": "new-pkg",
                    "version": "1.0.1",
                    "dist": {{
                        "tarball": "https://registry.npmjs.org/new-pkg/-/new-pkg-1.0.1.tgz",
                        "integrity": "sha512-def"
                    }}
                }}
            }},
            "time": {{
                "1.0.0": "{}",
                "1.0.1": "{}"
            }}
        }}"#,
        recent.to_rfc3339(),
        recent.to_rfc3339()
    );

    mount_package_with_time(&mock_server, "new-pkg", &test_pkg).await;

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "new-pkg".to_string(),
        requested: "^1.0.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let config = RegistryConfig::with_registry(mock_server.uri());
    let client = create_client()?;

    // With allow_new_packages = true, should get newest
    let maturity_config = MaturityConfig {
        minimum_age_minutes: 4320,
        excluded_packages: vec![],
        allow_new_packages: true, // Bypass!
    };
    let options = ResolveOptions {
        optimize: false,
        maturity: maturity_config,
        offline: false,
    };

    let calculated =
        calculate_depends_with_options(&client, &root, &dependencies, |_, _| {}, &config, &options)
            .await?;

    // Should get newest version despite being new
    let pkg = calculated
        .keys()
        .find(|d| d.name == "new-pkg")
        .expect("new-pkg should be resolved");
    assert_eq!(pkg.resolved, "1.0.1");

    // No fallback since we allowed new packages
    let info = calculated.get(pkg).unwrap();
    assert!(info.maturity_fallback.is_none());

    Ok(())
}

#[tokio::test]
async fn it_errors_when_all_versions_too_new() -> Result<(), Box<dyn std::error::Error>> {
    use chrono::{Duration, Utc};

    // Isolate from real cache
    let _temp = TempDir::new().unwrap();
    std::env::set_var("HOME", _temp.path());

    let mock_server = MockServer::start().await;

    // All versions published recently
    let recent = Utc::now() - Duration::hours(1);

    let test_pkg = format!(
        r#"{{
            "name": "brand-new-pkg",
            "versions": {{
                "1.0.0": {{
                    "name": "brand-new-pkg",
                    "version": "1.0.0",
                    "dist": {{
                        "tarball": "https://registry.npmjs.org/brand-new-pkg/-/brand-new-pkg-1.0.0.tgz",
                        "integrity": "sha512-xyz"
                    }}
                }}
            }},
            "time": {{
                "1.0.0": "{}"
            }}
        }}"#,
        recent.to_rfc3339()
    );

    mount_package_with_time(&mock_server, "brand-new-pkg", &test_pkg).await;

    let root = Dependency {
        name: "test-root".to_string(),
        requested: "1.0.0".to_string(),
        resolved: "1.0.0".to_string(),
        is_optional: false,
        alias: None,
        install_path: None,
    };

    let dependencies = vec![Dependency {
        name: "brand-new-pkg".to_string(),
        requested: "^1.0.0".to_string(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
        install_path: None,
    }];

    let config = RegistryConfig::with_registry(mock_server.uri());
    let client = create_client()?;

    let maturity_config = MaturityConfig {
        minimum_age_minutes: 4320, // 3 days
        excluded_packages: vec![],
        allow_new_packages: false,
    };
    let options = ResolveOptions {
        optimize: false,
        maturity: maturity_config,
        offline: false,
    };

    let result =
        calculate_depends_with_options(&client, &root, &dependencies, |_, _| {}, &config, &options)
            .await;

    // Should error because all versions are too new
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("No mature version"));

    Ok(())
}

/// Test that lockfile parsing correctly handles the same package@version at multiple paths.
/// This is a regression test for a bug where packages like fdir@6.5.0 would be deduplicated
/// even when needed at both node_modules/tinyglobby/node_modules/fdir AND
/// node_modules/vite/node_modules/fdir.
#[test]
fn test_lockfile_same_version_at_multiple_nested_paths() {
    use indexmap::IndexMap;
    use nary_lib::lockfile::{deps_from_lockfile, PackageEntry, PackageLock};

    // Simulate a lockfile where fdir@6.5.0 is needed at two different nested paths
    // This happens when parent packages have conflicting peer dependencies
    let mut packages = IndexMap::new();

    // Root entry
    packages.insert(
        "".to_string(),
        PackageEntry {
            version: Some("1.0.0".to_string()),
            ..Default::default()
        },
    );

    // Parent package 1: tinyglobby
    packages.insert(
        "node_modules/tinyglobby".to_string(),
        PackageEntry {
            version: Some("0.2.15".to_string()),
            resolved: Some(
                "https://registry.npmjs.org/tinyglobby/-/tinyglobby-0.2.15.tgz".to_string(),
            ),
            integrity: Some("sha512-abc".to_string()),
            ..Default::default()
        },
    );

    // Parent package 2: vite
    packages.insert(
        "node_modules/vite".to_string(),
        PackageEntry {
            version: Some("6.0.0".to_string()),
            resolved: Some("https://registry.npmjs.org/vite/-/vite-6.0.0.tgz".to_string()),
            integrity: Some("sha512-def".to_string()),
            ..Default::default()
        },
    );

    // fdir@6.5.0 nested under tinyglobby
    packages.insert(
        "node_modules/tinyglobby/node_modules/fdir".to_string(),
        PackageEntry {
            version: Some("6.5.0".to_string()),
            resolved: Some("https://registry.npmjs.org/fdir/-/fdir-6.5.0.tgz".to_string()),
            integrity: Some("sha512-fdir".to_string()),
            ..Default::default()
        },
    );

    // fdir@6.5.0 nested under vite (SAME version, different path)
    packages.insert(
        "node_modules/vite/node_modules/fdir".to_string(),
        PackageEntry {
            version: Some("6.5.0".to_string()),
            resolved: Some("https://registry.npmjs.org/fdir/-/fdir-6.5.0.tgz".to_string()),
            integrity: Some("sha512-fdir".to_string()),
            ..Default::default()
        },
    );

    let lock = PackageLock {
        name: Some("test-app".to_string()),
        version: Some("1.0.0".to_string()),
        lockfile_version: 3,
        requires: true,
        packages,
    };

    // Parse the lockfile
    let deps = deps_from_lockfile(&lock);

    // Should have 4 entries: tinyglobby, vite, and BOTH fdir instances
    assert_eq!(
        deps.len(),
        4,
        "Expected 4 packages but got {}. Packages: {:?}",
        deps.len(),
        deps.keys()
            .map(|d| format!("{}@{} at {:?}", d.name, d.resolved, d.install_path))
            .collect::<Vec<_>>()
    );

    // Count fdir entries - should be 2, not 1
    let fdir_count = deps.iter().filter(|(d, _)| d.name == "fdir").count();
    assert_eq!(
        fdir_count, 2,
        "Expected 2 fdir entries (one per nested path), got {}. \
         This would fail if Dependency equality ignores install_path.",
        fdir_count
    );

    // Verify both install paths are present
    let fdir_paths: Vec<&str> = deps
        .iter()
        .filter(|(d, _)| d.name == "fdir")
        .map(|(_, info)| info.install_path.as_str())
        .collect();

    assert!(
        fdir_paths.contains(&"node_modules/tinyglobby/node_modules/fdir"),
        "Missing fdir under tinyglobby. Found paths: {:?}",
        fdir_paths
    );
    assert!(
        fdir_paths.contains(&"node_modules/vite/node_modules/fdir"),
        "Missing fdir under vite. Found paths: {:?}",
        fdir_paths
    );

    // Verify the Dependency structs have install_path set (used for hash/eq)
    for (dep, info) in deps.iter().filter(|(d, _)| d.name == "fdir") {
        assert!(
            dep.install_path.is_some(),
            "fdir Dependency should have install_path set for lockfile entries"
        );
        assert_eq!(
            dep.install_path.as_ref().unwrap(),
            &info.install_path,
            "Dependency.install_path should match ResolvedInfo.install_path"
        );
    }
}
