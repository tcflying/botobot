//! §4.6 步③下半：交互原语（click / type）+ 坐标核心（移植 `.oni/agent-browser` interaction）。
//! 真坐标点击（`Input.dispatchMouseEvent`）/ 文本输入（`Input.insertText`）。
//!
//! **坐标核心** `quad_center` 纯函数可单测；click/type 的 CDP 发送 feature-gated、运行待真 Chrome。

/// DOM box 的 quad（CDP `DOM.getBoxModel` 的 `content`：8 个数 = 4 角点 x,y 顺时针）→ 中心点。
/// 取四角均值（对任意四边形稳健，比对角线中点更准）。
pub fn quad_center(quad: &[f64]) -> Option<(f64, f64)> {
    if quad.len() < 8 {
        return None;
    }
    let xs = quad[0] + quad[2] + quad[4] + quad[6];
    let ys = quad[1] + quad[3] + quad[5] + quad[7];
    Some((xs / 4.0, ys / 4.0))
}

#[cfg(feature = "browser")]
mod live {
    use serde_json::json;

    use super::super::page::Browser;

    impl Browser {
        /// 在真坐标 `(x,y)` 发一次完整鼠标点击（press+release）。需真 Chrome（运行待环境）。
        pub async fn click_at(
            &self,
            x: f64,
            y: f64,
            session_id: Option<&str>,
        ) -> Result<(), String> {
            let down = json!({
                "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1
            });
            let up = json!({
                "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1
            });
            self.conn()
                .send("Input.dispatchMouseEvent", down, session_id)
                .await?;
            self.conn()
                .send("Input.dispatchMouseEvent", up, session_id)
                .await?;
            Ok(())
        }

        /// 在当前焦点元素插入文本（`Input.insertText`，比逐键 dispatch 稳）。需真 Chrome。
        pub async fn type_text(&self, text: &str, session_id: Option<&str>) -> Result<(), String> {
            self.conn()
                .send("Input.insertText", json!({ "text": text }), session_id)
                .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_center_averages_corners() {
        // 矩形 (10,20)-(30,20)-(30,40)-(10,40) → 中心 (20,30)。
        let q = [10.0, 20.0, 30.0, 20.0, 30.0, 40.0, 10.0, 40.0];
        assert_eq!(quad_center(&q), Some((20.0, 30.0)));
    }

    #[test]
    fn quad_center_rejects_short() {
        assert_eq!(quad_center(&[1.0, 2.0, 3.0]), None);
    }
}
