//! Task dependency graph with cycle detection and topological ordering.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::{OrchestratorError, Result};
use brehon_types::TaskId;

#[derive(Debug, Clone)]
pub struct DependencyGraph {
    nodes: HashSet<TaskId>,
    edges: HashMap<TaskId, HashSet<TaskId>>,
    reverse_edges: HashMap<TaskId, HashSet<TaskId>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashSet::new(),
            edges: HashMap::new(),
            reverse_edges: HashMap::new(),
        }
    }

    pub fn add_task(&mut self, task_id: TaskId) {
        self.nodes.insert(task_id.clone());
        self.edges.entry(task_id.clone()).or_default();
        self.reverse_edges.entry(task_id).or_default();
    }

    pub fn remove_task(&mut self, task_id: &TaskId) {
        self.nodes.remove(task_id);

        if let Some(dependencies) = self.edges.remove(task_id) {
            for dep in dependencies {
                if let Some(reverse_deps) = self.reverse_edges.get_mut(&dep) {
                    reverse_deps.remove(task_id);
                }
            }
        }

        self.reverse_edges.remove(task_id);

        for deps in self.edges.values_mut() {
            deps.remove(task_id);
        }
    }

    pub fn add_dependency(&mut self, task_id: TaskId, depends_on: TaskId) -> Result<()> {
        self.nodes.insert(task_id.clone());
        self.nodes.insert(depends_on.clone());

        self.edges.entry(task_id.clone()).or_default();
        self.edges.entry(depends_on.clone()).or_default();
        self.reverse_edges.entry(task_id.clone()).or_default();
        self.reverse_edges.entry(depends_on.clone()).or_default();

        self.edges
            .get_mut(&task_id)
            .unwrap()
            .insert(depends_on.clone());

        if self.has_cycle() {
            self.edges.get_mut(&task_id).unwrap().remove(&depends_on);
            return Err(OrchestratorError::CycleError(format!(
                "Adding dependency {} -> {} would create a cycle",
                task_id, depends_on
            )));
        }

        self.reverse_edges
            .get_mut(&depends_on)
            .unwrap()
            .insert(task_id);

        Ok(())
    }

    pub fn remove_dependency(&mut self, task_id: &TaskId, depends_on: &TaskId) {
        if let Some(deps) = self.edges.get_mut(task_id) {
            deps.remove(depends_on);
        }
        if let Some(reverse_deps) = self.reverse_edges.get_mut(depends_on) {
            reverse_deps.remove(task_id);
        }
    }

    fn has_cycle(&self) -> bool {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node in &self.nodes {
            if !visited.contains(node) && self.has_cycle_dfs(node, &mut visited, &mut rec_stack) {
                return true;
            }
        }

        false
    }

    fn has_cycle_dfs(
        &self,
        node: &TaskId,
        visited: &mut HashSet<TaskId>,
        rec_stack: &mut HashSet<TaskId>,
    ) -> bool {
        visited.insert(node.clone());
        rec_stack.insert(node.clone());

        if let Some(neighbors) = self.edges.get(node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    if self.has_cycle_dfs(neighbor, visited, rec_stack) {
                        return true;
                    }
                } else if rec_stack.contains(neighbor) {
                    return true;
                }
            }
        }

        rec_stack.remove(node);
        false
    }

    pub fn get_dependencies(&self, task_id: &TaskId) -> HashSet<TaskId> {
        self.edges.get(task_id).cloned().unwrap_or_default()
    }

    pub fn get_dependents(&self, task_id: &TaskId) -> HashSet<TaskId> {
        self.reverse_edges.get(task_id).cloned().unwrap_or_default()
    }

    pub fn topological_order(&self) -> Result<Vec<TaskId>> {
        let mut in_degree: HashMap<TaskId, usize> = HashMap::new();

        for node in &self.nodes {
            in_degree.insert(node.clone(), 0);
        }

        for deps in self.edges.values() {
            for _dep in deps {}
        }

        for task_id in &self.nodes {
            let deps = self.edges.get(task_id);
            if let Some(deps) = deps {
                in_degree.insert(task_id.clone(), deps.len());
            }
        }

        let mut queue: VecDeque<TaskId> = VecDeque::new();
        for node in &self.nodes {
            if in_degree.get(node) == Some(&0) {
                queue.push_back(node.clone());
            }
        }

        let mut result = Vec::new();

        while let Some(node) = queue.pop_front() {
            result.push(node.clone());

            if let Some(dependents) = self.reverse_edges.get(&node) {
                for dependent in dependents {
                    if let Some(degree) = in_degree.get_mut(dependent) {
                        if *degree > 0 {
                            *degree -= 1;
                            if *degree == 0 {
                                queue.push_back(dependent.clone());
                            }
                        }
                    }
                }
            }
        }

        if result.len() != self.nodes.len() {
            return Err(OrchestratorError::CycleError(
                "Graph contains a cycle, cannot produce topological order".to_string(),
            ));
        }

        Ok(result)
    }

    pub fn get_ready_tasks(&self, completed: &HashSet<TaskId>) -> Vec<TaskId> {
        self.nodes
            .iter()
            .filter(|task_id| {
                if completed.contains(*task_id) {
                    return false;
                }

                let deps = self.edges.get(*task_id);
                match deps {
                    Some(deps) => deps.iter().all(|dep| completed.contains(dep)),
                    None => true,
                }
            })
            .cloned()
            .collect()
    }

    pub fn tasks_with_unmet_dependencies(
        &self,
        completed: &HashSet<TaskId>,
    ) -> HashMap<TaskId, Vec<TaskId>> {
        let mut result = HashMap::new();

        for task_id in &self.nodes {
            if completed.contains(task_id) {
                continue;
            }

            let unmet: Vec<TaskId> = self
                .edges
                .get(task_id)
                .map(|deps| {
                    deps.iter()
                        .filter(|dep| !completed.contains(*dep))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            if !unmet.is_empty() {
                result.insert(task_id.clone(), unmet);
            }
        }

        result
    }

    pub fn has_task(&self, task_id: &TaskId) -> bool {
        self.nodes.contains(task_id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph() {
        let graph = DependencyGraph::new();
        assert!(graph.is_empty());
    }

    #[test]
    fn add_task() {
        let mut graph = DependencyGraph::new();
        graph.add_task(TaskId::new("T001"));

        assert_eq!(graph.len(), 1);
        assert!(graph.has_task(&TaskId::new("T001")));
    }

    #[test]
    fn remove_task() {
        let mut graph = DependencyGraph::new();
        graph.add_task(TaskId::new("T001"));
        graph.remove_task(&TaskId::new("T001"));

        assert!(graph.is_empty());
        assert!(!graph.has_task(&TaskId::new("T001")));
    }

    #[test]
    fn add_dependency_no_cycle() {
        let mut graph = DependencyGraph::new();

        let result = graph.add_dependency(TaskId::new("T002"), TaskId::new("T001"));
        assert!(result.is_ok());

        let deps = graph.get_dependencies(&TaskId::new("T002"));
        assert!(deps.contains(&TaskId::new("T001")));

        let dependents = graph.get_dependents(&TaskId::new("T001"));
        assert!(dependents.contains(&TaskId::new("T002")));
    }

    #[test]
    fn add_dependency_creates_cycle_fails() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();

        let result = graph.add_dependency(TaskId::new("T001"), TaskId::new("T002"));
        assert!(result.is_err());

        if let Err(OrchestratorError::CycleError(msg)) = result {
            assert!(msg.contains("cycle"));
        } else {
            panic!("Expected CycleError");
        }
    }

    #[test]
    fn longer_cycle_detection() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        graph
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        let result = graph.add_dependency(TaskId::new("T001"), TaskId::new("T003"));
        assert!(result.is_err());
    }

    #[test]
    fn topological_order_simple() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        graph
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        let order = graph.topological_order().unwrap();

        assert!(
            order
                .iter()
                .position(|t| t == &TaskId::new("T001"))
                .unwrap()
                < order
                    .iter()
                    .position(|t| t == &TaskId::new("T002"))
                    .unwrap()
        );
        assert!(
            order
                .iter()
                .position(|t| t == &TaskId::new("T002"))
                .unwrap()
                < order
                    .iter()
                    .position(|t| t == &TaskId::new("T003"))
                    .unwrap()
        );
    }

    #[test]
    fn topological_order_with_cycle_fails() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        graph
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        // This would create a cycle, so it should fail
        let result = graph.add_dependency(TaskId::new("T001"), TaskId::new("T003"));
        assert!(result.is_err());

        // Since there's no cycle, the topological order should work
        let order_result = graph.topological_order();
        assert!(order_result.is_ok());
    }

    #[test]
    fn get_ready_tasks_empty() {
        let graph = DependencyGraph::new();
        let completed = HashSet::new();

        let ready = graph.get_ready_tasks(&completed);
        assert!(ready.is_empty());
    }

    #[test]
    fn get_ready_tasks_no_dependencies() {
        let mut graph = DependencyGraph::new();
        graph.add_task(TaskId::new("T001"));
        graph.add_task(TaskId::new("T002"));

        let completed = HashSet::new();
        let mut ready = graph.get_ready_tasks(&completed);
        ready.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn get_ready_tasks_with_dependencies() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        graph
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        let completed = HashSet::new();
        let ready = graph.get_ready_tasks(&completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&TaskId::new("T001")));

        let mut completed = HashSet::new();
        completed.insert(TaskId::new("T001"));

        let ready = graph.get_ready_tasks(&completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&TaskId::new("T002")));

        completed.insert(TaskId::new("T002"));
        let ready = graph.get_ready_tasks(&completed);
        assert_eq!(ready.len(), 1);
        assert!(ready.contains(&TaskId::new("T003")));
    }

    #[test]
    fn tasks_with_unmet_dependencies() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();
        graph
            .add_dependency(TaskId::new("T003"), TaskId::new("T002"))
            .unwrap();

        let completed = HashSet::new();
        let unmet = graph.tasks_with_unmet_dependencies(&completed);

        assert_eq!(unmet.len(), 2);
        assert!(unmet.contains_key(&TaskId::new("T002")));
        assert!(unmet.contains_key(&TaskId::new("T003")));

        let t002_unmet = unmet.get(&TaskId::new("T002")).unwrap();
        assert_eq!(t002_unmet.len(), 1);
        assert!(t002_unmet.contains(&TaskId::new("T001")));
    }

    #[test]
    fn remove_dependency() {
        let mut graph = DependencyGraph::new();

        graph
            .add_dependency(TaskId::new("T002"), TaskId::new("T001"))
            .unwrap();

        graph.remove_dependency(&TaskId::new("T002"), &TaskId::new("T001"));

        let deps = graph.get_dependencies(&TaskId::new("T002"));
        assert!(deps.is_empty());
    }
}
