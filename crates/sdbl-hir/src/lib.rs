mod context_detector;
mod diagnostics;
mod hir;
mod literal;
mod lower;
mod position_detector;
mod scope;
mod source_map;
mod standard_fields;
mod types;

pub use context_detector::{detect_context, is_mdo_type, parse_nested_column_ref};
pub use diagnostics::{
    ambiguous_qualified_head_message, invalid_virtual_table_parameters_message, LikeUsageKind,
    SdblDiagnostic, VirtualTableParameterProblem, SCHEDULE_DATA_NULL_MESSAGE,
};
pub use hir::{
    ExprHir, FieldDef, FieldHir, FunctionKind, GroupByHir, InValues, JoinHir, JoinType, Name,
    OrderByHir, OrderByItem, ParameterSource, ParameterSourceUse, ResolvedTable, SdblHir,
    SdblPackage, SdblQuery, SelectHir, TableRef, UnionHir, WhenClause,
};
pub use lower::{lower_sdbl_to_hir, lower_sdbl_to_hir_with_resolver, SdblLowerResult};
pub use position_detector::detect_sdbl_at_position;
pub use scope::Scope;
pub use source_map::{SdblSourceMap, TokenCategory, TokenInfo};
pub use types::{MdoRef, SdblType};

use text_size::TextSize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdblQueryInfo {
    pub query_text: String,
    pub offset_in_query: TextSize,
    pub bsl_literal_range: syntax::TextRange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdblCompletionContext {
    AfterFromKeyword,

    InsideMdoType {
        mdo_type: bsl_metadata::MdoType,
        prefix: String,
    },

    AfterMdoObject {
        mdo_type: bsl_metadata::MdoType,
        object_name: String,
        prefix: String,
    },

    SdblKeywords {
        prefix: String,
    },

    AfterTableAlias {
        alias: String,
        prefix: String,
    },

    AfterAsKeyword {
        context: AsContext,
        suggestion: Option<String>,
    },

    JoinTypeKeyword {
        prefix: String,
    },

    AfterOnKeyword {
        prefix: String,
    },

    AfterNestedField {
        alias: String,
        field_chain: Vec<String>,
        prefix: String,
    },

    AfterCastExpression {
        mdo_type: bsl_metadata::MdoType,
        object_name: String,
        field_chain: Vec<String>,
        prefix: String,
    },

    InsideValueFunction,

    InsideValueMdoType {
        mdo_type: bsl_metadata::MdoType,
        prefix: String,
        is_russian: bool,
    },

    InsideValueMdoObject {
        mdo_type: bsl_metadata::MdoType,
        object_name: String,
        prefix: String,
        is_russian: bool,
    },

    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsContext {
    InSelectField,

    InFromClause,

    InJoinClause,
}
