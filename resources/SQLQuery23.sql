RESTORE DATABASE [ADVWKS] FILE = N'AdventureWorks2025' FROM DISK = N'C:\Users\frank\Projects\tsql-server\resources\AdventureWorks2025.bak'
    WITH
    FILE = 1,
    NOUNLOAD,
    STATS = 10;


GO
