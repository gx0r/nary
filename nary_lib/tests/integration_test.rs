use nary_lib::deps::*;

use indoc::indoc;
use std::io::{Cursor};

use anyhow::{Result};

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
    let dependencies = json_to_dependencies(cursor);

    let dependencies = dependencies.unwrap();
    let dep = dependencies.get(0).unwrap();

    assert_eq!(dep.version, "^4.1.0");

}

#[test]
fn it_will_gather_dependencies() -> Result<()> {
    let koa_ejs = include_str!("repository/koa-ejs.json");
    let cursor = Cursor::new(koa_ejs);
    let dependencies = json_to_dependencies(cursor);

    let dependencies = dependencies?;
    assert_eq!(dependencies.get(0).unwrap().name, "debug");
    assert_eq!(dependencies.get(1).unwrap().name, "ejs");
    assert_eq!(dependencies.get(2).unwrap().name, "mz");

    Ok(())
}

#[test]
fn it_will_build_dependency_map() -> Result<()> {
    let koa_ejs = Cursor::new(include_str!("repository/koa-ejs.json"));
    let dependencies = json_to_dependencies(koa_ejs);

    let dependencies = dependencies?;
    assert_eq!(dependencies.get(0).unwrap().name, "debug");
    assert_eq!(dependencies.get(1).unwrap().name, "ejs");
    assert_eq!(dependencies.get(2).unwrap().name, "mz");

    let root = Dependency {
        name: "koa_ejs".to_string(),
        version: "1".to_string(),
    };

    let calculated = calculate_depends(&root, &dependencies)?;

    for dep in calculated {
        println!("{:?}", dep);
    }

    Ok(())
}