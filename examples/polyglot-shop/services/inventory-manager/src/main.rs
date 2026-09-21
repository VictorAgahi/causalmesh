// Inventory manager microservice listening to payment events and reserving stock

pub struct InventoryServiceServer;

impl InventoryServiceServer {
    pub fn reserve_stock(&self, order_id: &str, item_id: &str, qty: i32) -> bool {
        println!("[InventoryService.ReserveStock] Order: {order_id}, item: {item_id}, qty: {qty}");
        true
    }
}

pub struct InventoryConsumer {
    server: InventoryServiceServer,
}

impl InventoryConsumer {
    pub fn new() -> Self {
        Self {
            server: InventoryServiceServer,
        }
    }

    pub fn listen(&self) {
        // Subscribe to payment settlement event stream
        self.subscribe("payment-settled-topic");
    }

    fn subscribe(&self, topic: &str) {
        println!("Subscribed to {topic}");
        let reserved = self.server.reserve_stock("ord-9921", "sku-laptop-x1", 1);
        if reserved {
            self.publish("stock-reserved-topic");
        }
    }

    fn publish(&self, topic: &str) {
        println!("Published StockReservedEvent to {topic}");
    }
}

fn main() {
    let consumer = InventoryConsumer::new();
    consumer.listen();
}
