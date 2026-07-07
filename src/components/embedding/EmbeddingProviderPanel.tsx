import { useState, useCallback, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Box, Plus, Loader2 } from "lucide-react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { EmbeddingProviderCard } from "./EmbeddingProviderCard";
import { EmbeddingProviderFormModal } from "./EmbeddingProviderFormModal";
import { embeddingApi } from "@/lib/api";
import type { EmbeddingConfig, EmbeddingProvider } from "@/lib/api/embedding";

export function EmbeddingProviderPanel() {
  const { t } = useTranslation();

  const [config, setConfig] = useState<EmbeddingConfig>({ providers: {} });
  const [loading, setLoading] = useState(true);
  const [isFormOpen, setIsFormOpen] = useState(false);
  const [editingProvider, setEditingProvider] =
    useState<EmbeddingProvider | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState<{
    open: boolean;
    id: string;
    name: string;
  }>({ open: false, id: "", name: "" });

  const loadProviders = useCallback(async () => {
    try {
      setLoading(true);
      const data = await embeddingApi.getProviders();
      setConfig(data);
    } catch (error) {
      console.error("Failed to load embedding providers:", error);
      toast.error(
        error instanceof Error ? error.message : t("embeddingProvider.loadError", {
          defaultValue: "加载 Embedding Provider 失败",
        }),
      );
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    loadProviders();
  }, [loadProviders]);

  const handleSave = useCallback(
    async (provider: EmbeddingProvider) => {
      try {
        await embeddingApi.upsertProvider(provider);
        toast.success(
          editingProvider
            ? t("embeddingProvider.updated", {
                defaultValue: "Embedding Provider 已更新",
              })
            : t("embeddingProvider.added", {
                defaultValue: "Embedding Provider 已添加",
              }),
        );
        loadProviders();
        setEditingProvider(null);
      } catch (error) {
        console.error("Failed to save embedding provider:", error);
        toast.error(
          error instanceof Error ? error.message : t("embeddingProvider.saveError", {
            defaultValue: "保存 Embedding Provider 失败",
          }),
        );
      }
    },
    [editingProvider, loadProviders, t],
  );

  const handleDelete = useCallback(async () => {
    if (!deleteConfirm.id) return;

    try {
      await embeddingApi.deleteProvider(deleteConfirm.id);
      toast.success(
        t("embeddingProvider.deleted", {
          defaultValue: "Embedding Provider 已删除",
        }),
      );
      loadProviders();
    } catch (error) {
      console.error("Failed to delete embedding provider:", error);
      toast.error(
        t("embeddingProvider.deleteError", {
          defaultValue: "删除 Embedding Provider 失败",
        }),
      );
    } finally {
      setDeleteConfirm({ open: false, id: "", name: "" });
    }
  }, [deleteConfirm.id, loadProviders, t]);

  const handleSetCurrent = useCallback(
    async (id: string) => {
      try {
        await embeddingApi.setCurrentProvider(id);
        toast.success(
          t("embeddingProvider.setCurrent", {
            defaultValue: "已设为默认",
          }),
        );
        loadProviders();
      } catch (error) {
        console.error("Failed to set current embedding provider:", error);
        toast.error(
          t("embeddingProvider.setCurrentError", {
            defaultValue: "设置默认 Provider 失败",
          }),
        );
      }
    },
    [loadProviders, t],
  );

  const handleEdit = useCallback((provider: EmbeddingProvider) => {
    setEditingProvider(provider);
    setIsFormOpen(true);
  }, []);

  const handleDeleteClick = useCallback(
    (id: string) => {
      const provider = config.providers[id];
      setDeleteConfirm({
        open: true,
        id,
        name: provider?.name || id,
      });
    },
    [config.providers],
  );

  const providerList = Object.values(config.providers);

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Box className="h-5 w-5 text-primary" />
        <h2 className="text-lg font-semibold">
          {t("embeddingProvider.title", { defaultValue: "Embedding Provider" })}
        </h2>
        <span className="rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground">
          {providerList.length}
        </span>
      </div>

      <p className="text-sm text-muted-foreground">
        {t("embeddingProvider.description", {
          defaultValue:
            "管理 Embedding/Reranker 服务配置。用于 AI 应用的向量嵌入功能。",
        })}
      </p>

      {loading ? (
        <div className="flex items-center justify-center py-12">
          <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
        </div>
      ) : providerList.length === 0 ? (
        <div className="flex flex-col items-center justify-center rounded-xl border border-dashed py-12 text-center">
          <Box className="mb-3 h-10 w-10 text-muted-foreground/50" />
          <p className="text-sm text-muted-foreground">
            {t("embeddingProvider.empty", {
              defaultValue: "还没有 Embedding Provider",
            })}
          </p>
          <p className="mt-1 text-xs text-muted-foreground/70">
            {t("embeddingProvider.emptyHint", {
              defaultValue: "点击下方按钮创建一个",
            })}
          </p>
        </div>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {providerList.map((provider) => (
            <EmbeddingProviderCard
              key={provider.id}
              provider={provider}
              isCurrent={config.current === provider.id}
              onEdit={handleEdit}
              onDelete={handleDeleteClick}
              onSetCurrent={handleSetCurrent}
            />
          ))}
        </div>
      )}

      <div className="flex justify-end">
        <button
          onClick={() => {
            setEditingProvider(null);
            setIsFormOpen(true);
          }}
          className="inline-flex items-center gap-1 rounded-md bg-primary px-3 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90"
        >
          <Plus className="h-4 w-4" />
          {t("embeddingProvider.add", { defaultValue: "添加 Provider" })}
        </button>
      </div>

      <EmbeddingProviderFormModal
        isOpen={isFormOpen}
        onClose={() => {
          setIsFormOpen(false);
          setEditingProvider(null);
        }}
        onSave={handleSave}
        editingProvider={editingProvider}
      />

      <ConfirmDialog
        isOpen={deleteConfirm.open}
        title={t("embeddingProvider.deleteConfirmTitle", {
          defaultValue: "删除 Embedding Provider",
        })}
        message={t("embeddingProvider.deleteConfirmDescription", {
          defaultValue: `确定要删除 "${deleteConfirm.name}" 吗？`,
          name: deleteConfirm.name,
        })}
        confirmText={t("common.delete", { defaultValue: "删除" })}
        onConfirm={handleDelete}
        onCancel={() => setDeleteConfirm({ open: false, id: "", name: "" })}
      />
    </div>
  );
}
