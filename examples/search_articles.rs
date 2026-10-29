//! search_articles —— M5 示例：内嵌全文检索（Tantivy + jieba 中文分词）。
//!
//! 运行：`cargo run --example search_articles --features search`
//!
//! 演示：建库（首次）→ 灌数据 → 提交 → 中文检索 → schema 版本切换（rebuild + activate）。
//! 索引落在 `[search].index_dir`（默认 `data/search`），无需任何外部服务。

use aurora::prelude::*;
use aurora::search::{Query, SearchDocument, SearchEngine, SearchHit};
use serde::{Deserialize, Serialize};
use tantivy::schema::{STORED, Schema, TEXT};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Article {
    id: u64,
    title: String,
    body: String,
}

impl SearchDocument for Article {
    fn schema() -> Schema {
        let mut builder = Schema::builder();
        // 需要回读的字段必须 STORED（否则 search 反序列化会缺字段）
        builder.add_u64_field("id", STORED);
        builder.add_text_field("title", TEXT | STORED);
        builder.add_text_field("body", TEXT | STORED);
        builder.build()
    }
}

fn seed() -> Vec<Article> {
    vec![
        Article {
            id: 1,
            title: "你好，世界".to_string(),
            body: "内嵌全文检索，无需 Elasticsearch".to_string(),
        },
        Article {
            id: 2,
            title: "Rust 异步编程".to_string(),
            body: "Tokio 运行时、Future 与异步生态".to_string(),
        },
        Article {
            id: 3,
            title: "数据库连接池".to_string(),
            body: "SeaORM 与 SQLite 的配合使用".to_string(),
        },
    ]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = preset::load_env_var("AURORA_ENV", "config")?;
    init_tracing(&config.telemetry)?;

    let engine = SearchEngine::open(&config.search)?;
    tracing::info!(root = %engine.root().display(), alias = engine.alias(), "检索引擎就绪");

    // 首次运行：建库并激活；之后直接打开
    let index = match engine.index("articles") {
        Ok(index) => index,
        Err(_) => {
            tracing::info!("索引不存在，创建 v1");
            let created = engine.rebuild("articles", Article::schema()).await?;
            engine.activate(&created)?;
            created
        }
    };

    // 灌数据（示例每次运行都会追加；真实项目按业务增量写入）
    for article in seed() {
        index.add(&article)?;
    }
    index.commit()?;
    tracing::info!(
        count = index.count(),
        version = index.version(),
        "索引已提交"
    );

    // 中文检索（jieba 把「你好，世界」切成 你好 / 世界）
    // `&["title", "body"]` 是**允许字段集合**：用户输入里写 `other:xxx` 会被拒绝。
    for keyword in ["世界", "异步", "SQLite"] {
        let hits: Vec<SearchHit<Article>> = index
            .search(Query::parse(keyword, &["title", "body"]), 5)
            .await?;
        tracing::info!(%keyword, hits = hits.len(), "检索完成");
        for hit in hits {
            tracing::info!(score = hit.score, id = hit.doc.id, title = %hit.doc.title, "命中");
        }
    }

    // schema 变更演示：建 v2 → 重新灌 → 原子切换（查询无中断）
    let next = engine.rebuild("articles", Article::schema()).await?;
    for article in seed() {
        next.add(&article)?;
    }
    next.commit()?;
    engine.activate(&next)?;
    tracing::info!(version = %engine.index("articles")?.version(), "已切换到新版本");

    Ok(())
}
