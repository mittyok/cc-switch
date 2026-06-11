import { invoke } from "@tauri-apps/api/core";

export interface EmbeddingProvider {
  id: string;
  name: string;
  apiKey?: string;
  baseUrl: string;
  model: string;
  description?: string;
}

export interface EmbeddingConfig {
  providers: Record<string, EmbeddingProvider>;
  current?: string;
}

export const embeddingApi = {
  async getProviders(): Promise<EmbeddingConfig> {
    return await invoke("get_embedding_providers");
  },

  async upsertProvider(provider: EmbeddingProvider): Promise<void> {
    return await invoke("upsert_embedding_provider", { provider });
  },

  async deleteProvider(id: string): Promise<void> {
    return await invoke("delete_embedding_provider", { id });
  },

  async setCurrentProvider(id: string | null): Promise<void> {
    return await invoke("set_current_embedding_provider", { id });
  },
};
