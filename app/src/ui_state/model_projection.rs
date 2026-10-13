//! Shared view-model projection used by pointer-driven geometry updates.
use slint::{Model, ModelRc, VecModel};
pub fn sync<T: Clone + PartialEq + 'static>(current: ModelRc<T>, items: Vec<T>) -> ModelRc<T> {
    if let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() {
        let count = items.len();
        for (index, item) in items.into_iter().enumerate() {
            if index >= model.row_count() {
                model.push(item);
            } else if model.row_data(index).as_ref() != Some(&item) {
                model.set_row_data(index, item);
            }
        }
        while model.row_count() > count {
            model.remove(count);
        }
        current
    } else {
        ModelRc::new(VecModel::from(items))
    }
}
