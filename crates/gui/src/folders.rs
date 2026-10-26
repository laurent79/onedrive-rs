//! State of the folder chooser (selective sync): the folders listed so far,
//! what the user checked, and the lines to show. Folders are listed by the
//! daemon one level at a time, when opened.

use odrs_ipc::{Folder, FolderChoice};

struct Node {
    path: String,
    name: String,
    depth: usize,
    /// Its files and unlisted subfolders are synced.
    included: bool,
    /// Something below it differs (as the daemon said; cleared once changed here).
    mixed: bool,
    has_children: bool,
    /// Listed subfolders (`None`: not listed yet).
    children: Option<Vec<usize>>,
    expanded: bool,
    loading: bool,
}

/// A line of the chooser, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    pub name: String,
    pub depth: usize,
    /// `Some(true)`: synced, `Some(false)`: not, `None`: partly.
    pub check: Option<bool>,
    pub expanded: bool,
    pub has_children: bool,
    pub loading: bool,
}

#[derive(Default)]
pub struct FolderTree {
    /// Drive being edited.
    pub drive: String,
    nodes: Vec<Node>,
    /// The top-level folders, once listed.
    top: Option<Vec<usize>>,
    /// Node of each line shown.
    lines: Vec<usize>,
}

impl FolderTree {
    pub fn new(drive: &str) -> Self {
        Self { drive: drive.to_owned(), ..Self::default() }
    }

    /// Subfolders of `path` (empty: the top) as listed by the daemon. Below a
    /// folder with nothing different inside (or changed here), they follow it.
    pub fn add_children(&mut self, path: &str, folders: Vec<Folder>) {
        let parent = if path.is_empty() { None } else { self.nodes.iter().position(|n| n.path == path) };
        if !path.is_empty() && parent.is_none() {
            return;
        }
        let inherit = parent.map(|p| &self.nodes[p]).filter(|p| !p.mixed).map(|p| p.included);
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        let mut ids = Vec::new();
        for f in folders {
            ids.push(self.nodes.len());
            self.nodes.push(Node {
                path: f.path,
                name: f.name,
                depth,
                included: inherit.unwrap_or(f.included),
                mixed: inherit.is_none() && f.mixed,
                has_children: f.has_children,
                children: None,
                expanded: false,
                loading: false,
            });
        }
        match parent {
            None => self.top = Some(ids),
            Some(p) => {
                let node = &mut self.nodes[p];
                node.has_children = !ids.is_empty();
                node.children = Some(ids);
                node.expanded = true;
                node.loading = false;
            }
        }
    }

    /// Listing a folder failed: let the user try again.
    pub fn listing_failed(&mut self, path: &str) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.path == path) {
            n.loading = false;
        }
    }

    pub fn is_listed(&self) -> bool {
        self.top.is_some()
    }

    /// Open or close the folder of a line. `Some(path)`: its subfolders must be listed first.
    pub fn expand(&mut self, line: usize) -> Option<String> {
        let node = &mut self.nodes[*self.lines.get(line)?];
        if node.children.is_none() {
            if node.loading {
                return None;
            }
            node.loading = true;
            return Some(node.path.clone());
        }
        node.expanded = !node.expanded;
        None
    }

    /// Check or uncheck a line's folder and everything inside it.
    pub fn toggle(&mut self, line: usize) {
        let Some(&id) = self.lines.get(line) else { return };
        let target = self.state(id) != Some(true);
        let mut stack = vec![id];
        while let Some(i) = stack.pop() {
            let node = &mut self.nodes[i];
            node.included = target;
            node.mixed = false;
            stack.extend(node.children.iter().flatten());
        }
    }

    fn state(&self, id: usize) -> Option<bool> {
        let node = &self.nodes[id];
        match &node.children {
            None if node.mixed => None,
            None => Some(node.included),
            Some(children) => children.iter().all(|&c| self.state(c) == Some(node.included)).then_some(node.included),
        }
    }

    /// The lines to show (open folders show their subfolders).
    pub fn lines(&mut self) -> Vec<Line> {
        let mut ids = Vec::new();
        let mut stack: Vec<usize> = self.top.iter().flatten().rev().copied().collect();
        while let Some(id) = stack.pop() {
            ids.push(id);
            let node = &self.nodes[id];
            if node.expanded {
                stack.extend(node.children.iter().flatten().rev());
            }
        }
        self.lines = ids;
        self.lines
            .iter()
            .map(|&id| {
                let n = &self.nodes[id];
                Line {
                    path: n.path.clone(),
                    name: n.name.clone(),
                    depth: n.depth,
                    check: self.state(id),
                    expanded: n.expanded,
                    has_children: n.has_children,
                    loading: n.loading,
                }
            })
            .collect()
    }

    /// Something would be synced (a folder checked, or partly below a folder not opened).
    pub fn anything_selected(&self) -> bool {
        self.nodes.iter().any(|n| n.included || (n.mixed && n.children.is_none()))
    }

    pub fn choices(&self) -> Vec<FolderChoice> {
        self.nodes
            .iter()
            .map(|n| FolderChoice { path: n.path.clone(), included: n.included, mixed: n.mixed, expanded: n.children.is_some() })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(path: &str, included: bool, mixed: bool) -> Folder {
        let name = path.rsplit('/').next().unwrap().to_owned();
        Folder { name, path: path.into(), included, mixed, has_children: true }
    }

    #[test]
    fn check_states_follow_the_tree() {
        let mut t = FolderTree::new("main");
        t.add_children("", vec![folder("Documents", true, false), folder("Teams", false, true)]);
        let lines = t.lines();
        assert_eq!(lines.iter().map(|l| l.check).collect::<Vec<_>>(), [Some(true), None]);

        // Opening Teams: listing first, then its subfolders keep the daemon's states.
        assert_eq!(t.expand(1), Some("Teams".to_owned()));
        assert!(t.lines()[1].loading);
        t.add_children("Teams", vec![folder("Teams/Engineering", true, false), folder("Teams/Sales", false, false)]);
        let lines = t.lines();
        assert_eq!(lines.iter().map(|l| (l.depth, l.check)).collect::<Vec<_>>(), [(0, Some(true)), (0, None), (1, Some(true)), (1, Some(false))]);

        // Checking a partly synced folder checks everything in it.
        t.toggle(1);
        assert!(t.lines().iter().all(|l| l.check == Some(true)));
        // Unchecking a subfolder makes its parent partly synced.
        t.toggle(3);
        assert_eq!(t.lines()[1].check, None);
        let teams = t.choices().into_iter().find(|c| c.path == "Teams").unwrap();
        assert!(teams.included && !teams.mixed && teams.expanded);

        // Closing and opening again needs no listing.
        assert_eq!(t.expand(1), None);
        assert_eq!(t.lines().len(), 2);
        assert_eq!(t.expand(1), None);
        assert_eq!(t.lines().len(), 4);
    }

    #[test]
    fn subfolders_follow_a_changed_parent() {
        let mut t = FolderTree::new("main");
        t.add_children("", vec![folder("Teams", false, true)]);
        t.lines();
        t.toggle(0);
        assert_eq!(t.expand(0), Some("Teams".to_owned()));
        // The daemon still describes the old rules: ignored below a changed folder.
        t.add_children("Teams", vec![folder("Teams/Sales", false, false)]);
        assert!(t.lines().iter().all(|l| l.check == Some(true)));
        t.toggle(0);
        assert!(!t.anything_selected());
    }
}
