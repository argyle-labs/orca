let open = $state(true);
let sections = $state<string[]>(['orca']);
// Spec repos linked to a project — hidden from the API DOCS list.
let linkedSpecRepos = $state<string[]>([]);
// Schema db names linked to a project — hidden from the SCHEMA list.
let linkedSchemas = $state<string[]>([]);
// Service ids (within a server) linked to a project — hidden from the SERVICES list.
let linkedServiceIds = $state<string[]>([]);

export function getSidebarOpen() {
  return open;
}
export function setSidebarOpen(v: boolean) {
  open = v;
}
export function toggleSidebar() {
  open = !open;
}

export function getSidebarSections() {
  return sections;
}
export function setSidebarSections(s: string[]) {
  sections = s;
}

export function getLinkedSpecRepos() {
  return linkedSpecRepos;
}
export function setLinkedSpecRepos(repos: string[]) {
  linkedSpecRepos = repos;
}

export function getLinkedSchemas() {
  return linkedSchemas;
}
export function setLinkedSchemas(names: string[]) {
  linkedSchemas = names;
}

export function getLinkedServiceIds() {
  return linkedServiceIds;
}
export function setLinkedServiceIds(ids: string[]) {
  linkedServiceIds = ids;
}

// Full project list written by ProjectsPanel — read by ServicesPanel to find linked projects.
import type { Project } from '../types/sidebar';
let allProjects = $state<Project[]>([]);
// Project names owned by a service — written by ServicesPanel, read by ProjectsPanel to filter.
let linkedProjectNames = $state<string[]>([]);

export function getAllProjects() {
  return allProjects;
}
export function setAllProjects(p: Project[]) {
  allProjects = p;
}
export function getLinkedProjectNames() {
  return linkedProjectNames;
}
export function setLinkedProjectNames(names: string[]) {
  linkedProjectNames = names;
}
