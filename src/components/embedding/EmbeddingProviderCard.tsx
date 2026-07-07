import { useTranslation } from "react-i18next";
import { Box, Pencil, Trash2, Check } from "lucide-react";
import type { EmbeddingProvider } from "@/lib/api/embedding";

interface Props {
  provider: EmbeddingProvider;
  isCurrent: boolean;
  onEdit: (provider: EmbeddingProvider) => void;
  onDelete: (id: string) => void;
  onSetCurrent: (id: string) => void;
}

export function EmbeddingProviderCard({
  provider,
  isCurrent,
  onEdit,
  onDelete,
  onSetCurrent,
}: Props) {
  const { t } = useTranslation();

  return (
    <div className="group relative rounded-lg border bg-card p-4 shadow-sm">
      <div className="flex items-start justify-between">
        <div className="flex items-center gap-2">
          <Box className="h-5 w-5 text-muted-foreground" />
          <div>
            <h3 className="font-medium">{provider.name}</h3>
            {isCurrent && (
              <span className="inline-flex items-center gap-1 rounded-full bg-green-100 px-2 py-0.5 text-xs font-medium text-green-700 dark:bg-green-900/30 dark:text-green-400">
                <Check className="h-3 w-3" />
                {t("embeddingProvider.current", { defaultValue: "默认" })}
              </span>
            )}
          </div>
        </div>
        <div className="flex items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100">
          {!isCurrent && (
            <button
              onClick={() => onSetCurrent(provider.id)}
              className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
              title={t("embeddingProvider.setAsCurrent", { defaultValue: "设为默认" })}
            >
              <Check className="h-4 w-4" />
            </button>
          )}
          <button
            onClick={() => onEdit(provider)}
            className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground"
            title={t("common.edit", { defaultValue: "编辑" })}
          >
            <Pencil className="h-4 w-4" />
          </button>
          <button
            onClick={() => onDelete(provider.id)}
            className="rounded p-1 text-muted-foreground hover:bg-muted hover:text-destructive"
            title={t("common.delete", { defaultValue: "删除" })}
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>

      <div className="mt-3 space-y-1 text-xs text-muted-foreground">
        <p>
          <span className="font-medium">URL:</span> {provider.baseUrl}
        </p>
        <p>
          <span className="font-medium">Model:</span> {provider.model}
        </p>
        {provider.description && (
          <p className="line-clamp-2">{provider.description}</p>
        )}
      </div>
    </div>
  );
}
