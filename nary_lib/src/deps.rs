use anyhow::{anyhow, Result};

use petgraph;
use petgraph::graphmap::DiGraphMap;

use bidir_map::BidirMap;

use indexmap::IndexMap;
use serde_json::Value;
use std::{cmp::Ordering, fs::File, io, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Package {
    name: String,
    version: String,
}

impl Package {
    pub fn from_json(buffer: &str) -> Result<Self> {
        let root: Value = serde_json::from_str(&buffer)?;
        let name = root["name"].as_str().unwrap_or_default().to_string();
        let version = root["version"].as_str().unwrap_or_default().to_string();

        Ok(Package {
            name: name.clone(),
            version: version.clone(),
        })
    }
}

impl Ord for Package {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.name.cmp(&other.name))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PackageInfo {
    reference: usize,
}

impl Ord for PackageInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        self.reference.cmp(&other.reference)
    }
}

impl PartialOrd for PackageInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.reference.cmp(&other.reference))
    }
}

type DependencyId = i32;

pub fn calculate_depends(
    root_pkg: &Dependency,
    deps: &Vec<Dependency>,
) -> Result<IndexMap<Dependency, ()>> {
    let mut graph: DiGraphMap<DependencyId, i32> = DiGraphMap::new();

    // String doesn't implement Copy and graphmap requires Copy
    let mut map: BidirMap<Dependency, DependencyId> = BidirMap::new();

    map.insert(root_pkg.clone(), 0);

    calculate_depends_rec(root_pkg, deps, &mut map, &mut graph)?;

    let dependency_ids = petgraph::algo::toposort(&graph, None).or_else(|err| {
        Err(anyhow!("Cyclic dependency {:?}", map.get_by_second(&err.node_id())))
    })?;

    let mut ordered_dependencies: IndexMap<Dependency, ()> = IndexMap::new();

    println!("Deps: {:?}", ordered_dependencies);

    for i in dependency_ids {
        let second = map.get_by_second(&i).unwrap();

        if !ordered_dependencies.contains_key(second) {
            if let Some((dep, _)) = map.remove_by_second(&i) {
                ordered_dependencies.insert(dep.clone(), ());
            }
        }
    }

    Ok(ordered_dependencies)
}

pub fn calculate_depends_rec(
    dependency: &Dependency,
    deps: &Vec<Dependency>,
    map: &mut BidirMap<Dependency, DependencyId>,
    graph: &mut DiGraphMap<DependencyId, i32>,
) -> Result<()> {
    let curr_node = *map.get_by_first(dependency).unwrap();

    if deps.len() == 0 {
        return Ok(());
    }

    let mut remaining_deps = deps.clone();

    while !remaining_deps.is_empty() {
        let index = remaining_deps.len() - 1;
        let dependency = remaining_deps.remove(index);

        if !map.contains_first_key(&dependency) {
            let dependency_node = map.len() as i32;
            graph.add_node(dependency_node);
            map.insert(dependency.clone(), dependency_node);

            graph.add_edge(dependency_node, curr_node, 0);
            let dependency = map.get_mut_by_second(&dependency_node).unwrap().clone();

            calculate_depends_rec(&dependency, &remaining_deps, map, graph)?;
        } else {
            let dependency_node = *map.get_by_first(&dependency).unwrap();
            graph.add_edge(dependency_node, curr_node, 0);
        }
    }

    Ok(())
}

pub fn path_to_root_dependency<'a>(file: &Path) -> Result<Dependency> {
    let mut package = file.to_path_buf();

    if !package.ends_with("package.json") {
        package.push("package.json");
    }

    let package_json = File::open(package)?;
    let root: Value = serde_json::from_reader(package_json)?;

    Ok(Dependency {
        name: root["name"].as_str().unwrap().to_string(),
        version: root["version"].as_str().unwrap().to_string()
    })
}

pub fn path_to_dependencies<'a>(file: &Path) -> Result<Vec<Dependency>> {
    let mut package = file.to_path_buf();

    if !package.ends_with("package.json") {
        package.push("package.json");
    }

    let package_json = File::open(package)?;

    json_to_dependencies(&package_json)
}

pub fn json_to_dependencies(mut reader: impl io::Read) -> Result<Vec<Dependency>> {
    let mut buffer = String::new();
    reader.read_to_string(&mut buffer)?;

    let root: Value = serde_json::from_str(&buffer)?;
    let mut vec = Vec::new();

    if let Some(dependencies) = root["dependencies"].as_object() {
        for dependency in dependencies.iter() {
            vec.push(Dependency {
                name: dependency.0.to_string(),
                version: dependency.1.as_str().unwrap().to_string(),
            });
        }
    };

    Ok(vec)
}
