//! Contains basic data about various HIR declarations.

use std::{mem, sync::Arc};

use hir_expand::{name::Name, AstId, ExpandResult, HirFileId, InFile, MacroCallId};
use syntax::ast;

use crate::{
    attr::Attrs,
    body::{Expander, Mark},
    db::DefDatabase,
    intern::Interned,
    item_tree::{self, AssocItem, FnFlags, ItemTreeId, ModItem, Param, TreeId},
    nameres::{attr_resolution::ResolvedAttr, DefMap},
    type_ref::{TraitRef, TypeBound, TypeRef},
    visibility::RawVisibility,
    AssocItemId, AstIdWithPath, ConstId, ConstLoc, FunctionId, FunctionLoc, HasModule, ImplId,
    Intern, ItemContainerId, Lookup, ModuleId, StaticId, TraitId, TypeAliasId, TypeAliasLoc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionData {
    pub name: Name,
    pub params: Vec<(Option<Name>, Interned<TypeRef>)>,
    pub ret_type: Interned<TypeRef>,
    pub async_ret_type: Option<Interned<TypeRef>>,
    pub attrs: Attrs,
    pub visibility: RawVisibility,
    pub abi: Option<Interned<str>>,
    pub legacy_const_generics_indices: Vec<u32>,
    flags: FnFlags,
}

impl FunctionData {
    pub(crate) fn fn_data_query(db: &dyn DefDatabase, func: FunctionId) -> Arc<FunctionData> {
        let loc = func.lookup(db);
        let krate = loc.container.module(db).krate;
        let crate_graph = db.crate_graph();
        let cfg_options = &crate_graph[krate].cfg_options;
        let item_tree = loc.id.item_tree(db);
        let func = &item_tree[loc.id.value];

        let enabled_params = func
            .params
            .clone()
            .filter(|&param| item_tree.attrs(db, krate, param.into()).is_cfg_enabled(cfg_options));

        // If last cfg-enabled param is a `...` param, it's a varargs function.
        let is_varargs = enabled_params
            .clone()
            .next_back()
            .map_or(false, |param| matches!(item_tree[param], Param::Varargs));

        let mut flags = func.flags;
        if is_varargs {
            flags.bits |= FnFlags::IS_VARARGS;
        }

        if matches!(loc.container, ItemContainerId::ExternBlockId(_)) {
            flags.bits |= FnFlags::IS_IN_EXTERN_BLOCK;
        }

        let legacy_const_generics_indices = item_tree
            .attrs(db, krate, ModItem::from(loc.id.value).into())
            .by_key("rustc_legacy_const_generics")
            .tt_values()
            .next()
            .map(|arg| parse_rustc_legacy_const_generics(arg))
            .unwrap_or_default();

        Arc::new(FunctionData {
            name: func.name.clone(),
            params: enabled_params
                .clone()
                .filter_map(|id| match &item_tree[id] {
                    Param::Normal(name, ty) => Some((name.clone(), ty.clone())),
                    Param::Varargs => None,
                })
                .collect(),
            ret_type: func.ret_type.clone(),
            async_ret_type: func.async_ret_type.clone(),
            attrs: item_tree.attrs(db, krate, ModItem::from(loc.id.value).into()),
            visibility: item_tree[func.visibility].clone(),
            abi: func.abi.clone(),
            legacy_const_generics_indices,
            flags,
        })
    }

    pub fn has_body(&self) -> bool {
        self.flags.bits & FnFlags::HAS_BODY != 0
    }

    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.flags.bits & FnFlags::HAS_SELF_PARAM != 0
    }

    pub fn is_default(&self) -> bool {
        self.flags.bits & FnFlags::IS_DEFAULT != 0
    }

    pub fn is_const(&self) -> bool {
        self.flags.bits & FnFlags::IS_CONST != 0
    }

    pub fn is_async(&self) -> bool {
        self.flags.bits & FnFlags::IS_ASYNC != 0
    }

    pub fn is_unsafe(&self) -> bool {
        self.flags.bits & FnFlags::IS_UNSAFE != 0
    }

    pub fn is_in_extern_block(&self) -> bool {
        self.flags.bits & FnFlags::IS_IN_EXTERN_BLOCK != 0
    }

    pub fn is_varargs(&self) -> bool {
        self.flags.bits & FnFlags::IS_VARARGS != 0
    }
}

fn parse_rustc_legacy_const_generics(tt: &tt::Subtree) -> Vec<u32> {
    let mut indices = Vec::new();
    for args in tt.token_trees.chunks(2) {
        match &args[0] {
            tt::TokenTree::Leaf(tt::Leaf::Literal(lit)) => match lit.text.parse() {
                Ok(index) => indices.push(index),
                Err(_) => break,
            },
            _ => break,
        }

        if let Some(comma) = args.get(1) {
            match comma {
                tt::TokenTree::Leaf(tt::Leaf::Punct(punct)) if punct.char == ',' => {}
                _ => break,
            }
        }
    }

    indices
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAliasData {
    pub name: Name,
    pub type_ref: Option<Interned<TypeRef>>,
    pub visibility: RawVisibility,
    pub is_extern: bool,
    /// Bounds restricting the type alias itself (eg. `type Ty: Bound;` in a trait or impl).
    pub bounds: Vec<Interned<TypeBound>>,
}

impl TypeAliasData {
    pub(crate) fn type_alias_data_query(
        db: &dyn DefDatabase,
        typ: TypeAliasId,
    ) -> Arc<TypeAliasData> {
        let loc = typ.lookup(db);
        let item_tree = loc.id.item_tree(db);
        let typ = &item_tree[loc.id.value];

        Arc::new(TypeAliasData {
            name: typ.name.clone(),
            type_ref: typ.type_ref.clone(),
            visibility: item_tree[typ.visibility].clone(),
            is_extern: matches!(loc.container, ItemContainerId::ExternBlockId(_)),
            bounds: typ.bounds.to_vec(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitData {
    pub name: Name,
    pub items: Vec<(Name, AssocItemId)>,
    pub is_auto: bool,
    pub is_unsafe: bool,
    pub visibility: RawVisibility,
    /// Whether the trait has `#[rust_skip_array_during_method_dispatch]`. `hir_ty` will ignore
    /// method calls to this trait's methods when the receiver is an array and the crate edition is
    /// 2015 or 2018.
    pub skip_array_during_method_dispatch: bool,
    // box it as the vec is usually empty anyways
    pub attribute_calls: Option<Box<Vec<(AstId<ast::Item>, MacroCallId)>>>,
}

impl TraitData {
    pub(crate) fn trait_data_query(db: &dyn DefDatabase, tr: TraitId) -> Arc<TraitData> {
        let tr_loc = tr.lookup(db);
        let item_tree = tr_loc.id.item_tree(db);
        let tr_def = &item_tree[tr_loc.id.value];
        let _cx = stdx::panic_context::enter(format!(
            "trait_data_query({:?} -> {:?} -> {:?})",
            tr, tr_loc, tr_def
        ));
        let name = tr_def.name.clone();
        let is_auto = tr_def.is_auto;
        let is_unsafe = tr_def.is_unsafe;
        let module_id = tr_loc.container;
        let visibility = item_tree[tr_def.visibility].clone();
        let skip_array_during_method_dispatch = item_tree
            .attrs(db, tr_loc.container.krate(), ModItem::from(tr_loc.id.value).into())
            .by_key("rustc_skip_array_during_method_dispatch")
            .exists();

        let mut collector = AssocItemCollector::new(
            db,
            module_id,
            tr_loc.id.file_id(),
            ItemContainerId::TraitId(tr),
        );
        collector.collect(tr_loc.id.tree_id(), &tr_def.items);

        Arc::new(TraitData {
            name,
            attribute_calls: collector.take_attr_calls(),
            items: collector.items,
            is_auto,
            is_unsafe,
            visibility,
            skip_array_during_method_dispatch,
        })
    }

    pub fn associated_types(&self) -> impl Iterator<Item = TypeAliasId> + '_ {
        self.items.iter().filter_map(|(_name, item)| match item {
            AssocItemId::TypeAliasId(t) => Some(*t),
            _ => None,
        })
    }

    pub fn associated_type_by_name(&self, name: &Name) -> Option<TypeAliasId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::TypeAliasId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    pub fn method_by_name(&self, name: &Name) -> Option<FunctionId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::FunctionId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }

    pub fn attribute_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.attribute_calls.iter().flat_map(|it| it.iter()).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplData {
    pub target_trait: Option<Interned<TraitRef>>,
    pub self_ty: Interned<TypeRef>,
    pub items: Vec<AssocItemId>,
    pub is_negative: bool,
    // box it as the vec is usually empty anyways
    pub attribute_calls: Option<Box<Vec<(AstId<ast::Item>, MacroCallId)>>>,
}

impl ImplData {
    pub(crate) fn impl_data_query(db: &dyn DefDatabase, id: ImplId) -> Arc<ImplData> {
        let _p = profile::span("impl_data_query");
        let impl_loc = id.lookup(db);

        let item_tree = impl_loc.id.item_tree(db);
        let impl_def = &item_tree[impl_loc.id.value];
        let target_trait = impl_def.target_trait.clone();
        let self_ty = impl_def.self_ty.clone();
        let is_negative = impl_def.is_negative;
        let module_id = impl_loc.container;

        let mut collector = AssocItemCollector::new(
            db,
            module_id,
            impl_loc.id.file_id(),
            ItemContainerId::ImplId(id),
        );
        collector.collect(impl_loc.id.tree_id(), &impl_def.items);

        let attribute_calls = collector.take_attr_calls();
        let items = collector.items.into_iter().map(|(_, item)| item).collect();

        Arc::new(ImplData { target_trait, self_ty, items, is_negative, attribute_calls })
    }

    pub fn attribute_calls(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.attribute_calls.iter().flat_map(|it| it.iter()).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstData {
    /// `None` for `const _: () = ();`
    pub name: Option<Name>,
    pub type_ref: Interned<TypeRef>,
    pub visibility: RawVisibility,
}

impl ConstData {
    pub(crate) fn const_data_query(db: &dyn DefDatabase, konst: ConstId) -> Arc<ConstData> {
        let loc = konst.lookup(db);
        let item_tree = loc.id.item_tree(db);
        let konst = &item_tree[loc.id.value];

        Arc::new(ConstData {
            name: konst.name.clone(),
            type_ref: konst.type_ref.clone(),
            visibility: item_tree[konst.visibility].clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticData {
    pub name: Name,
    pub type_ref: Interned<TypeRef>,
    pub visibility: RawVisibility,
    pub mutable: bool,
    pub is_extern: bool,
}

impl StaticData {
    pub(crate) fn static_data_query(db: &dyn DefDatabase, konst: StaticId) -> Arc<StaticData> {
        let loc = konst.lookup(db);
        let item_tree = loc.id.item_tree(db);
        let statik = &item_tree[loc.id.value];

        Arc::new(StaticData {
            name: statik.name.clone(),
            type_ref: statik.type_ref.clone(),
            visibility: item_tree[statik.visibility].clone(),
            mutable: statik.mutable,
            is_extern: matches!(loc.container, ItemContainerId::ExternBlockId(_)),
        })
    }
}

struct AssocItemCollector<'a> {
    db: &'a dyn DefDatabase,
    module_id: ModuleId,
    def_map: Arc<DefMap>,
    container: ItemContainerId,
    expander: Expander,

    items: Vec<(Name, AssocItemId)>,
    attr_calls: Vec<(AstId<ast::Item>, MacroCallId)>,
}

impl<'a> AssocItemCollector<'a> {
    fn new(
        db: &'a dyn DefDatabase,
        module_id: ModuleId,
        file_id: HirFileId,
        container: ItemContainerId,
    ) -> Self {
        Self {
            db,
            module_id,
            def_map: module_id.def_map(db),
            container,
            expander: Expander::new(db, file_id, module_id),

            items: Vec::new(),
            attr_calls: Vec::new(),
        }
    }

    fn take_attr_calls(&mut self) -> Option<Box<Vec<(AstId<ast::Item>, MacroCallId)>>> {
        let attribute_calls = mem::take(&mut self.attr_calls);
        if attribute_calls.is_empty() {
            None
        } else {
            Some(Box::new(attribute_calls))
        }
    }

    fn collect(&mut self, tree_id: TreeId, assoc_items: &[AssocItem]) {
        let item_tree = tree_id.item_tree(self.db);

        'items: for &item in assoc_items {
            let attrs = item_tree.attrs(self.db, self.module_id.krate, ModItem::from(item).into());
            if !attrs.is_cfg_enabled(self.expander.cfg_options()) {
                continue;
            }

            for attr in &*attrs {
                let ast_id =
                    AstId::new(self.expander.current_file_id(), item.ast_id(&item_tree).upcast());
                let ast_id_with_path = AstIdWithPath { path: (*attr.path).clone(), ast_id };

                if let Ok(ResolvedAttr::Macro(call_id)) = self.def_map.resolve_attr_macro(
                    self.db,
                    self.module_id.local_id,
                    ast_id_with_path,
                    attr,
                ) {
                    self.attr_calls.push((ast_id, call_id));
                    let res = self.expander.enter_expand_id(self.db, call_id);
                    self.collect_macro_items(res);
                    continue 'items;
                }
            }

            match item {
                AssocItem::Function(id) => {
                    let item = &item_tree[id];
                    let def =
                        FunctionLoc { container: self.container, id: ItemTreeId::new(tree_id, id) }
                            .intern(self.db);
                    self.items.push((item.name.clone(), def.into()));
                }
                AssocItem::Const(id) => {
                    let item = &item_tree[id];
                    let name = match item.name.clone() {
                        Some(name) => name,
                        None => continue,
                    };
                    let def =
                        ConstLoc { container: self.container, id: ItemTreeId::new(tree_id, id) }
                            .intern(self.db);
                    self.items.push((name, def.into()));
                }
                AssocItem::TypeAlias(id) => {
                    let item = &item_tree[id];
                    let def = TypeAliasLoc {
                        container: self.container,
                        id: ItemTreeId::new(tree_id, id),
                    }
                    .intern(self.db);
                    self.items.push((item.name.clone(), def.into()));
                }
                AssocItem::MacroCall(call) => {
                    let call = &item_tree[call];
                    let ast_id_map = self.db.ast_id_map(self.expander.current_file_id());
                    let root = self.db.parse_or_expand(self.expander.current_file_id()).unwrap();
                    let call = ast_id_map.get(call.ast_id).to_node(&root);
                    let _cx =
                        stdx::panic_context::enter(format!("collect_items MacroCall: {}", call));
                    let res = self.expander.enter_expand(self.db, call);

                    if let Ok(res) = res {
                        self.collect_macro_items(res);
                    }
                }
            }
        }
    }

    fn collect_macro_items(&mut self, res: ExpandResult<Option<(Mark, ast::MacroItems)>>) {
        if let Some((mark, mac)) = res.value {
            let src: InFile<ast::MacroItems> = self.expander.to_source(mac);
            let tree_id = item_tree::TreeId::new(src.file_id, None);
            let item_tree = tree_id.item_tree(self.db);
            let iter: Vec<_> =
                item_tree.top_level_items().iter().filter_map(ModItem::as_assoc_item).collect();

            self.collect(tree_id, &iter);

            self.expander.exit(self.db, mark);
        }
    }
}
