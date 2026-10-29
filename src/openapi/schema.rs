//! 统一响应体 [`ApiResponse<T>`](crate::http::ApiResponse) 的 OpenAPI schema。
//!
//! 契约见 `docs/09-error-and-response-spec.md` §1：`{ code, message, data, trace_id }`。
//!
//! `ApiResponse<T>` 是泛型，若沿用默认 `name()`（只取类型名）会让
//! `ApiResponse<User>` 与 `ApiResponse<Order>` 撞名，故此处显式命名为
//! `ApiResponse_{T}`，并让 `data` 引用 `T` 自身的 schema——这样文档中
//! `data` 能正确展开为业务模型（`docs/03` §4.16）。
//!
//! 实现要点：utoipa 5.5 中泛型类型要实现的是 [`ComposeSchema`](utoipa::__dev::ComposeSchema)
//! ——`PartialSchema` 由 `impl<T: ComposeSchema + ?Sized> PartialSchema for T` 自动派生，
//! 且 `#[utoipa::path(body = ...)]` 也要求 `ComposeSchema`。

use std::borrow::Cow;

use utoipa::openapi::schema::{AnyOf, ObjectBuilder, Type};
use utoipa::openapi::{KnownFormat, Ref, RefOr, Schema, SchemaFormat};
use utoipa::{PartialSchema, ToSchema};

use crate::http::ApiResponse;

/// `data` 的 schema：`T | null`（OpenAPI 3.0 用 `anyOf` 表达可空）。
///
/// 失败响应里 `data` 为 `null`（见 `docs/09` §1），故不能只写 `T`；
/// 用 `$ref` 指向组件里已登记的 `T`，避免在文档中重复内联展开。
fn nullable_data<T: ToSchema>() -> RefOr<Schema> {
    let mut any_of = AnyOf::new();
    any_of
        .items
        .push(RefOr::Ref(Ref::from_schema_name(T::name())));
    any_of
        .items
        .push(ObjectBuilder::new().schema_type(Type::Null).into());
    RefOr::T(Schema::AnyOf(any_of))
}

impl<T: ToSchema> utoipa::__dev::ComposeSchema for ApiResponse<T> {
    fn compose(_new_generics: Vec<RefOr<Schema>>) -> RefOr<Schema> {
        ObjectBuilder::new()
            .property(
                "code",
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
                    .description(Some("业务错误码；0 表示成功（判断一律用 code）")),
            )
            .required("code")
            .property(
                "message",
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .description(Some("人类可读信息；500 类不暴露内部细节")),
            )
            .required("message")
            .property("data", nullable_data::<T>())
            .property(
                "trace_id",
                ObjectBuilder::new()
                    .schema_type(Type::String)
                    .description(Some("32 位十六进制 W3C trace-id")),
            )
            .required("trace_id")
            .into()
    }
}

impl<T: ToSchema> ToSchema for ApiResponse<T> {
    /// 泛型命名：`ApiResponse_{T}`。
    ///
    /// 注意（`docs/07` §3.18）：`ToSchema::name()` 不携带泛型实参，容器类型（如
    /// `Vec<T>` / `Option<T>`）会退化为外层名（`Vec` / `Option`），因此
    /// `ApiResponse<Vec<User>>` 与 `ApiResponse<Vec<Order>>` 会撞名。需要区分时，
    /// 业务侧可用命名 newtype 包裹（`struct UserList(Vec<User>)`）。
    fn name() -> Cow<'static, str> {
        Cow::Owned(format!("ApiResponse_{}", T::name()))
    }

    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        // 先递归收集 `T` 自身引用的 schema，再登记 `T` 与 `Self`。
        <T as ToSchema>::schemas(schemas);
        schemas.push((T::name().into_owned(), T::schema()));
        schemas.push((Self::name().into_owned(), Self::schema()));
    }
}

#[cfg(test)]
mod tests {
    use utoipa::ToSchema;

    use super::*;

    #[derive(ToSchema)]
    #[allow(dead_code)]
    struct User {
        id: i64,
        email: String,
    }

    #[test]
    fn should_name_generic_envelope_per_inner_type() {
        assert_eq!(ApiResponse::<User>::name(), "ApiResponse_User");
        assert_eq!(ApiResponse::<String>::name(), "ApiResponse_String");
    }

    #[test]
    fn should_reference_inner_schema_from_data_property() {
        let schema = serde_json::to_value(ApiResponse::<User>::schema()).unwrap();
        let props = &schema["properties"];

        assert_eq!(props["code"]["type"], "integer");
        assert_eq!(props["code"]["format"], "int32");
        assert_eq!(props["message"]["type"], "string");
        assert_eq!(props["trace_id"]["type"], "string");
        // data 为 `T | null`：anyOf 里含内部类型的 $ref 与 null 分支
        let any_of = props["data"]["anyOf"].as_array().expect("data.anyOf");
        assert_eq!(any_of[0]["$ref"], "#/components/schemas/User");
        assert_eq!(any_of[1]["type"], "null");
    }

    #[test]
    fn should_mark_required_fields() {
        let schema = serde_json::to_value(ApiResponse::<User>::schema()).unwrap();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();

        assert_eq!(required, vec!["code", "message", "trace_id"]);
    }

    #[test]
    fn should_register_inner_and_envelope_schemas() {
        let mut schemas: Vec<(String, RefOr<Schema>)> = Vec::new();
        ApiResponse::<User>::schemas(&mut schemas);
        let names: Vec<&str> = schemas.iter().map(|(n, _)| n.as_str()).collect();

        assert!(names.contains(&"User"), "内部模型 schema 应被登记");
        assert!(
            names.contains(&"ApiResponse_User"),
            "统一响应体 schema 应被登记"
        );
    }
}
