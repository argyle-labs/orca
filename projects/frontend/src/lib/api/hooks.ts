// ⚠️  AUTO-GENERATED — do not edit. Run `orca gen` to regenerate.

import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import type { UseQueryOptions, UseMutationOptions } from '@tanstack/react-query';
import type * as T from './types';
import * as client from './client';
import { staleMs } from './stale';

export function useListBitbucketPRs(params: Parameters<typeof client.listBitbucketPRs>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listBitbucketPRs', params],
    queryFn: () => client.listBitbucketPRs(params),
    ...options,
    enabled: !!params,
  });
}

export function useListBitbucketRepos(options?: Omit<UseQueryOptions<T.RepoInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listBitbucketRepos'],
    queryFn: () => client.listBitbucketRepos(),
    ...options,
  });
}

export function useSearchConfluence(params: Parameters<typeof client.searchConfluence>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['searchConfluence', params],
    queryFn: () => client.searchConfluence(params),
    ...options,
  });
}

export function useGetLibraryDocs(params: Parameters<typeof client.getLibraryDocs>[0], options?: Omit<UseQueryOptions<T.Ctx7Response>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getLibraryDocs', params],
    queryFn: () => client.getLibraryDocs(params),
    staleTime: staleMs(300000),
    ...options,
    enabled: !!params,
  });
}

export function useGetDoc(params: Parameters<typeof client.getDoc>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getDoc', params],
    queryFn: () => client.getDoc(params),
    staleTime: staleMs(300000),
    ...options,
    enabled: !!params,
  });
}

export function useRunDockerAction(options?: UseMutationOptions<T.DockerActionResponse, Error, Parameters<typeof client.runDockerAction>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.runDockerAction>[0]) => client.runDockerAction(params),
    ...options,
  });
}

export function useGetDockerEngine(options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getDockerEngine'],
    queryFn: () => client.getDockerEngine(),
    ...options,
  });
}

export function useStartDockerEngine(options?: UseMutationOptions<void, Error, void>) {
  return useMutation({
    mutationFn: (_: void) => client.startDockerEngine(),
    ...options,
  });
}

export function useListDockerRuntimes(options?: Omit<UseQueryOptions<T.DockerRuntimeInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listDockerRuntimes'],
    queryFn: () => client.listDockerRuntimes(),
    ...options,
  });
}

export function useAddDockerRuntime(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.addDockerRuntime>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.addDockerRuntime>[0]) => client.addDockerRuntime(params),
    ...options,
  });
}

export function useRemoveDockerRuntime(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.removeDockerRuntime>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.removeDockerRuntime>[0]) => client.removeDockerRuntime(params),
    ...options,
  });
}

export function useGetDockerServices(params: Parameters<typeof client.getDockerServices>[0], options?: Omit<UseQueryOptions<T.DockerServicesResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getDockerServices', params],
    queryFn: () => client.getDockerServices(params),
    ...options,
    enabled: !!params,
  });
}

export function useFsBrowse(params: Parameters<typeof client.fsBrowse>[0], options?: Omit<UseQueryOptions<T.FsBrowseResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['fsBrowse', params],
    queryFn: () => client.fsBrowse(params),
    ...options,
  });
}

export function useListGithubOrgs(options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listGithubOrgs'],
    queryFn: () => client.listGithubOrgs(),
    ...options,
  });
}

export function useListGithubRepos(params: Parameters<typeof client.listGithubRepos>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listGithubRepos', params],
    queryFn: () => client.listGithubRepos(params),
    ...options,
  });
}

export function useListGithubIssues(params: Parameters<typeof client.listGithubIssues>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listGithubIssues', params],
    queryFn: () => client.listGithubIssues(params),
    ...options,
  });
}

export function useListGithubPRs(params: Parameters<typeof client.listGithubPRs>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listGithubPRs', params],
    queryFn: () => client.listGithubPRs(params),
    ...options,
  });
}

export function useGetGithubUser(options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getGithubUser'],
    queryFn: () => client.getGithubUser(),
    ...options,
  });
}

export function usePing(options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['ping'],
    queryFn: () => client.ping(),
    ...options,
  });
}

export function useListJiraIssues(params: Parameters<typeof client.listJiraIssues>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listJiraIssues', params],
    queryFn: () => client.listJiraIssues(params),
    ...options,
  });
}

export function useGetJiraTransitions(params: Parameters<typeof client.getJiraTransitions>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getJiraTransitions', params],
    queryFn: () => client.getJiraTransitions(params),
    ...options,
  });
}

export function useTransitionJiraIssue(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.transitionJiraIssue>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.transitionJiraIssue>[0]) => client.transitionJiraIssue(params),
    ...options,
  });
}

export function useGetLearningProgress(options?: Omit<UseQueryOptions<T.ProgressResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getLearningProgress'],
    queryFn: () => client.getLearningProgress(),
    ...options,
  });
}

export function useSaveLearningProgress(options?: UseMutationOptions<void, Error, Parameters<typeof client.saveLearningProgress>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.saveLearningProgress>[0]) => client.saveLearningProgress(params),
    ...options,
  });
}

export function useGetLogs(params: Parameters<typeof client.getLogs>[0], options?: Omit<UseQueryOptions<T.LogsResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getLogs', params],
    queryFn: () => client.getLogs(params),
    ...options,
    enabled: !!params,
  });
}

export function useGetLogServices(options?: Omit<UseQueryOptions<T.LogServicesResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getLogServices'],
    queryFn: () => client.getLogServices(),
    ...options,
  });
}

export function useListMcpMappings(params: Parameters<typeof client.listMcpMappings>[0], options?: Omit<UseQueryOptions<T.MappingRow[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listMcpMappings', params],
    queryFn: () => client.listMcpMappings(params),
    ...options,
  });
}

export function useCreateMcpMapping(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.createMcpMapping>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.createMcpMapping>[0]) => client.createMcpMapping(params),
    ...options,
  });
}

export function useDeleteMcpMapping(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.deleteMcpMapping>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.deleteMcpMapping>[0]) => client.deleteMcpMapping(params),
    ...options,
  });
}

export function useRunMcpTool(options?: UseMutationOptions<T.McpRunResponse, Error, Parameters<typeof client.runMcpTool>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.runMcpTool>[0]) => client.runMcpTool(params),
    ...options,
  });
}

export function useListMcpServers(options?: Omit<UseQueryOptions<T.McpServerInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listMcpServers'],
    queryFn: () => client.listMcpServers(),
    ...options,
  });
}

export function useAddMcpServer(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.addMcpServer>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.addMcpServer>[0]) => client.addMcpServer(params),
    ...options,
  });
}

export function useRemoveMcpServer(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.removeMcpServer>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.removeMcpServer>[0]) => client.removeMcpServer(params),
    ...options,
  });
}

export function useGetMcpTools(options?: Omit<UseQueryOptions<T.McpToolInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getMcpTools'],
    queryFn: () => client.getMcpTools(),
    ...options,
  });
}

export function useDownloadPdf(params: Parameters<typeof client.downloadPdf>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['downloadPdf', params],
    queryFn: () => client.downloadPdf(params),
    staleTime: staleMs(300000),
    ...options,
    enabled: !!params,
  });
}

export function useListPlugins(options?: Omit<UseQueryOptions<T.PluginInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listPlugins'],
    queryFn: () => client.listPlugins(),
    ...options,
  });
}

export function useInstallPlugin(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.installPlugin>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.installPlugin>[0]) => client.installPlugin(params),
    ...options,
  });
}

export function useRemovePlugin(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.removePlugin>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.removePlugin>[0]) => client.removePlugin(params),
    ...options,
  });
}

export function useListPluginCreds(params: Parameters<typeof client.listPluginCreds>[0], options?: Omit<UseQueryOptions<T.CredInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listPluginCreds', params],
    queryFn: () => client.listPluginCreds(params),
    ...options,
  });
}

export function useSetPluginCred(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.setPluginCred>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.setPluginCred>[0]) => client.setPluginCred(params),
    ...options,
  });
}

export function useSyncPluginCreds(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.syncPluginCreds>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.syncPluginCreds>[0]) => client.syncPluginCreds(params),
    ...options,
  });
}

export function useDeletePluginCred(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.deletePluginCred>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.deletePluginCred>[0]) => client.deletePluginCred(params),
    ...options,
  });
}

export function useListPluginData(params: Parameters<typeof client.listPluginData>[0], options?: Omit<UseQueryOptions<T.PluginDataEntry[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listPluginData', params],
    queryFn: () => client.listPluginData(params),
    ...options,
  });
}

export function useGetPluginData(params: Parameters<typeof client.getPluginData>[0], options?: Omit<UseQueryOptions<T.PluginDataEntry>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getPluginData', params],
    queryFn: () => client.getPluginData(params),
    ...options,
  });
}

export function useSetPluginData(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.setPluginData>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.setPluginData>[0]) => client.setPluginData(params),
    ...options,
  });
}

export function useDeletePluginData(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.deletePluginData>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.deletePluginData>[0]) => client.deletePluginData(params),
    ...options,
  });
}

export function useDisablePlugin(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.disablePlugin>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.disablePlugin>[0]) => client.disablePlugin(params),
    ...options,
  });
}

export function useEnablePlugin(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.enablePlugin>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.enablePlugin>[0]) => client.enablePlugin(params),
    ...options,
  });
}

export function useGetPluginHealth(params: Parameters<typeof client.getPluginHealth>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getPluginHealth', params],
    queryFn: () => client.getPluginHealth(params),
    ...options,
  });
}

export function useGetHealth(options?: Omit<UseQueryOptions<T.HealthResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getHealth'],
    queryFn: () => client.getHealth(),
    ...options,
  });
}

export function useGetSchema(options?: Omit<UseQueryOptions<T.SchemaResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSchema'],
    queryFn: () => client.getSchema(),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useListSchemaDatabases(options?: Omit<UseQueryOptions<T.SchemaDbInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listSchemaDatabases'],
    queryFn: () => client.listSchemaDatabases(),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useAddSchemaDatabase(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.addSchemaDatabase>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.addSchemaDatabase>[0]) => client.addSchemaDatabase(params),
    ...options,
  });
}

export function useRemoveSchemaDatabase(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.removeSchemaDatabase>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.removeSchemaDatabase>[0]) => client.removeSchemaDatabase(params),
    ...options,
  });
}

export function useGetSchemaDomains(options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSchemaDomains'],
    queryFn: () => client.getSchemaDomains(),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useSearchDocs(params: Parameters<typeof client.searchDocs>[0], options?: Omit<UseQueryOptions<T.SearchResult[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['searchDocs', params],
    queryFn: () => client.searchDocs(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useListSpecs(options?: Omit<UseQueryOptions<T.SpecMeta[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listSpecs'],
    queryFn: () => client.listSpecs(),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useListDbSpecs(options?: Omit<UseQueryOptions<T.SpecInfo[]>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['listDbSpecs'],
    queryFn: () => client.listDbSpecs(),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useRegisterSpec(options?: UseMutationOptions<T.SpecInfo, Error, Parameters<typeof client.registerSpec>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.registerSpec>[0]) => client.registerSpec(params),
    ...options,
  });
}

export function useSyncMcpSpecs(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.syncMcpSpecs>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.syncMcpSpecs>[0]) => client.syncMcpSpecs(params),
    ...options,
  });
}

export function useRefreshSpec(options?: UseMutationOptions<T.SpecInfo, Error, Parameters<typeof client.refreshSpec>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.refreshSpec>[0]) => client.refreshSpec(params),
    ...options,
  });
}

export function useUnregisterSpec(options?: UseMutationOptions<T.OkResponse, Error, Parameters<typeof client.unregisterSpec>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.unregisterSpec>[0]) => client.unregisterSpec(params),
    ...options,
  });
}

export function useGetSpec(params: Parameters<typeof client.getSpec>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSpec', params],
    queryFn: () => client.getSpec(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useDownloadSpec(params: Parameters<typeof client.downloadSpec>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['downloadSpec', params],
    queryFn: () => client.downloadSpec(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useGetSpecGraphql(params: Parameters<typeof client.getSpecGraphql>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSpecGraphql', params],
    queryFn: () => client.getSpecGraphql(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useDownloadGraphql(params: Parameters<typeof client.downloadGraphql>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['downloadGraphql', params],
    queryFn: () => client.downloadGraphql(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useGetSpecGraphqlInfo(params: Parameters<typeof client.getSpecGraphqlInfo>[0], options?: Omit<UseQueryOptions<T.GraphQlInfo>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSpecGraphqlInfo', params],
    queryFn: () => client.getSpecGraphqlInfo(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useProxyGraphql(options?: UseMutationOptions<void, Error, Parameters<typeof client.proxyGraphql>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.proxyGraphql>[0]) => client.proxyGraphql(params),
    ...options,
  });
}

export function useGetSpecPublic(params: Parameters<typeof client.getSpecPublic>[0], options?: Omit<UseQueryOptions<void>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getSpecPublic', params],
    queryFn: () => client.getSpecPublic(params),
    staleTime: staleMs(300000),
    ...options,
  });
}

export function useSystem_action_handler(options?: UseMutationOptions<T.SystemActionResponse, Error, Parameters<typeof client.system_action_handler>[0]>) {
  return useMutation({
    mutationFn: (params: Parameters<typeof client.system_action_handler>[0]) => client.system_action_handler(params),
    ...options,
  });
}

export function useSystem_status_handler(options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['system_status_handler'],
    queryFn: () => client.system_status_handler(),
    ...options,
  });
}

export function useRunTests(params: Parameters<typeof client.runTests>[0], options?: Omit<UseQueryOptions<T.TestRunResponse>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['runTests', params],
    queryFn: () => client.runTests(params),
    ...options,
    enabled: !!params,
  });
}

export function useGetTree(params: Parameters<typeof client.getTree>[0], options?: Omit<UseQueryOptions<unknown>, 'queryKey' | 'queryFn'>) {
  return useQuery({
    queryKey: ['getTree', params],
    queryFn: () => client.getTree(params),
    staleTime: staleMs(300000),
    ...options,
  });
}
