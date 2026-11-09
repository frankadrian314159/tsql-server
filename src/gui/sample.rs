//! Sample data for trying the application.

pub const SAMPLE_SQL: &str = "
CREATE TABLE dbo.Customers (
    CustomerId INT IDENTITY(1,1) PRIMARY KEY,
    Name NVARCHAR(60) NOT NULL,
    City NVARCHAR(40) NULL,
    Email VARCHAR(80) NULL
)
GO
CREATE TABLE dbo.Products (
    ProductId INT IDENTITY(1,1) PRIMARY KEY,
    Name NVARCHAR(60) NOT NULL,
    Price DECIMAL(10,2) NOT NULL,
    Stock INT NOT NULL DEFAULT 0
)
GO
CREATE TABLE dbo.Orders (
    OrderId INT IDENTITY(1,1) PRIMARY KEY,
    CustomerId INT NOT NULL,
    Ordered DATE NOT NULL,
    CONSTRAINT FK_Orders_Customers FOREIGN KEY (CustomerId) REFERENCES dbo.Customers(CustomerId)
)
GO
CREATE TABLE dbo.OrderLines (
    OrderId INT NOT NULL,
    Line INT NOT NULL,
    ProductId INT NOT NULL,
    Quantity INT NOT NULL,
    CONSTRAINT PK_OrderLines PRIMARY KEY (OrderId, Line),
    CONSTRAINT FK_Lines_Orders FOREIGN KEY (OrderId) REFERENCES dbo.Orders(OrderId),
    CONSTRAINT FK_Lines_Products FOREIGN KEY (ProductId) REFERENCES dbo.Products(ProductId)
)
GO
CREATE INDEX IX_Orders_Customer ON dbo.Orders (CustomerId)
GO
INSERT dbo.Customers (Name, City, Email) VALUES
    (N'Ada Lovelace', N'London', 'ada@example.com'),
    (N'Grace Hopper', N'New York', 'grace@example.com'),
    (N'Alan Turing', N'Manchester', NULL),
    (N'Edsger Dijkstra', N'Eindhoven', 'edsger@example.com')
INSERT dbo.Products (Name, Price, Stock) VALUES
    (N'Keyboard', 49.90, 25), (N'Monitor', 189.00, 8), (N'Mouse', 19.50, 60), (N'Cable', 4.25, 300)
INSERT dbo.Orders (CustomerId, Ordered) VALUES (1, '2024-01-15'), (2, '2024-02-03'), (1, '2024-03-21'), (3, '2024-03-22')
INSERT dbo.OrderLines (OrderId, Line, ProductId, Quantity) VALUES
    (1, 1, 1, 1), (1, 2, 3, 2), (2, 1, 2, 1), (3, 1, 4, 10), (3, 2, 3, 1), (4, 1, 2, 2)
GO
CREATE VIEW dbo.OrderTotals AS
    SELECT o.OrderId, c.Name AS Customer, o.Ordered, SUM(l.Quantity * p.Price) AS Total
    FROM dbo.Orders o
    JOIN dbo.Customers c ON c.CustomerId = o.CustomerId
    JOIN dbo.OrderLines l ON l.OrderId = o.OrderId
    JOIN dbo.Products p ON p.ProductId = l.ProductId
    GROUP BY o.OrderId, c.Name, o.Ordered
GO
CREATE PROCEDURE dbo.CustomerOrders @CustomerId INT AS
    SELECT * FROM dbo.OrderTotals WHERE Customer = (SELECT Name FROM dbo.Customers WHERE CustomerId = @CustomerId)
GO
";
