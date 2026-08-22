using System;
using System.Collections.Generic;
using Repo = Acme.Data.UserRepository;

namespace Acme.Services;

/// <summary>Manages users.</summary>
[Serializable]
public class UserService : BaseService, IUserService, IDisposable
{
    private readonly Repo _repo;
    public const int MaxUsers = 100;

    public UserService(Repo repo)
    {
        _repo = repo;
    }

    public string Name { get; set; }

    public User Find(int id)
    {
        Log("find");
        return _repo.Load(id);
    }

    public User Find(string email, bool exact = true)
    {
        var u = new User(email);
        return this.Validate(u);
    }

    private User Validate(User u) => u;

    public async Task<int> CountAsync() { return await _repo.CountAsync(); }

    public void Dispose() { }
}

public class BaseService
{
    protected void Log(string message) { }
}

public interface IUserService
{
    User Find(int id);
}

public record User(string Email);

public enum Role { Admin, Member }

public struct Point { public int X; }
