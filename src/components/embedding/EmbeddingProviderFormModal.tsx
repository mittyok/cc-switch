import { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import type { EmbeddingProvider } from "@/lib/api/embedding";

interface Props {
  isOpen: boolean;
  onClose: () => void;
  onSave: (provider: EmbeddingProvider) => void;
  editingProvider: EmbeddingProvider | null;
}

export function EmbeddingProviderFormModal({
  isOpen,
  onClose,
  onSave,
  editingProvider,
}: Props) {
  const { t } = useTranslation();

  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState("text-embedding-3-small");
  const [description, setDescription] = useState("");

  useEffect(() => {
    if (editingProvider) {
      setId(editingProvider.id);
      setName(editingProvider.name);
      setBaseUrl(editingProvider.baseUrl);
      setApiKey(editingProvider.apiKey || "");
      setModel(editingProvider.model);
      setDescription(editingProvider.description || "");
    } else {
      setId(crypto.randomUUID());
      setName("");
      setBaseUrl("");
      setApiKey("");
      setModel("text-embedding-3-small");
      setDescription("");
    }
  }, [editingProvider, isOpen]);

  if (!isOpen) return null;

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();

    if (!name.trim() || !baseUrl.trim()) {
      return;
    }

    onSave({
      id,
      name: name.trim(),
      baseUrl: baseUrl.trim(),
      apiKey: apiKey.trim() || undefined,
      model: model.trim() || "text-embedding-3-small",
      description: description.trim() || undefined,
    });
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/50" onClick={onClose} />
      <div className="relative z-10 w-full max-w-md rounded-lg bg-background p-6 shadow-lg">
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold">
            {editingProvider
              ? t("embeddingProvider.edit", { defaultValue: "编辑 Provider" })
              : t("embeddingProvider.add", { defaultValue: "添加 Provider" })}
          </h2>
          <button
            onClick={onClose}
            className="rounded p-1 text-muted-foreground hover:bg-muted"
          >
            <X className="h-5 w-5" />
          </button>
        </div>

        <form onSubmit={handleSubmit} className="mt-4 space-y-4">
          <div>
            <label className="block text-sm font-medium">
              {t("embeddingProvider.name", { defaultValue: "名称" })}
            </label>
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="OpenAI"
              className="mt-1 w-full rounded-md border bg-background px-3 py-2 text-sm"
              required
            />
          </div>

          <div>
            <label className="block text-sm font-medium">
              {t("embeddingProvider.baseUrl", { defaultValue: "Base URL" })}
            </label>
            <input
              type="url"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder="https://api.openai.com/v1"
              className="mt-1 w-full rounded-md border bg-background px-3 py-2 text-sm"
              required
            />
          </div>

          <div>
            <label className="block text-sm font-medium">
              {t("embeddingProvider.apiKey", { defaultValue: "API Key" })}
            </label>
            <input
              type="password"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              placeholder="sk-..."
              className="mt-1 w-full rounded-md border bg-background px-3 py-2 text-sm"
            />
          </div>

          <div>
            <label className="block text-sm font-medium">
              {t("embeddingProvider.model", { defaultValue: "Model" })}
            </label>
            <input
              type="text"
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder="text-embedding-3-small"
              className="mt-1 w-full rounded-md border bg-background px-3 py-2 text-sm"
            />
          </div>

          <div>
            <label className="block text-sm font-medium">
              {t("embeddingProvider.description", { defaultValue: "描述" })}
            </label>
            <textarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder={t("embeddingProvider.descriptionPlaceholder", {
                defaultValue: "可选描述",
              })}
              className="mt-1 w-full rounded-md border bg-background px-3 py-2 text-sm"
              rows={2}
            />
          </div>

          <div className="flex justify-end gap-2">
            <button
              type="button"
              onClick={onClose}
              className="rounded-md border px-4 py-2 text-sm hover:bg-muted"
            >
              {t("common.cancel", { defaultValue: "取消" })}
            </button>
            <button
              type="submit"
              className="rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90"
            >
              {t("common.save", { defaultValue: "保存" })}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
